//! TUI render (ratatui) and event loop (crossterm).

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

use crate::app::{App, BoardCard, BoardFocus, InputKind, SessionKind, Tab, status_label};
use jaum_flows::review::{ConstraintResult, ConstraintVerdict};

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
        app.poll_job();
        app.tick_reload();
        app.tick_pr_sync();
        app.tick_toast();
        sync_pty_size(terminal, app);
        terminal.draw(|f| render(f, app))?;

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

/// Like `sync_pty_size`, but from an explicit size (used by the daemon, which
/// renders to an in-memory backend without a real terminal). Syncs only the
/// SELECTED (displayed) session with the master-detail terminal pane.
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

/// Handles a mouse event over the Session tab: if claude is in mouse mode
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
    // 0) job log overlay (ingest/capture/init)
    if app.job_overlay {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.dismiss_job(),
            KeyCode::Char('k') | KeyCode::Up => app.job_scroll_up(),
            KeyCode::Char('j') | KeyCode::Down => app.job_scroll_down(),
            KeyCode::Char('g') | KeyCode::Home => app.job_scroll_top(),
            KeyCode::Char('G') | KeyCode::End => app.job_follow(),
            _ => {}
        }
        return;
    }

    // 0) project picker (overlay)
    if app.project_picker {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_picker(),
            KeyCode::Char('j') | KeyCode::Down => app.picker_next(),
            KeyCode::Char('k') | KeyCode::Up => app.picker_prev(),
            KeyCode::Enter => app.confirm_picker(),
            _ => {}
        }
        return;
    }

    // 0.5) task detail overlay
    if app.detail_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.close_detail(),
            KeyCode::Char('j') | KeyCode::Down => app.detail_scroll_down(),
            KeyCode::Char('k') | KeyCode::Up => app.detail_scroll_up(),
            _ => {}
        }
        return;
    }

    // 0.6) doc view overlay (markdown)
    if app.doc_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_doc(),
            KeyCode::Char('j') | KeyCode::Down => app.doc_scroll_down(),
            KeyCode::Char('k') | KeyCode::Up => app.doc_scroll_up(),
            _ => {}
        }
        return;
    }

    // 1) text capture (defer / convention / new task)
    if app.input.is_some() {
        match key.code {
            KeyCode::Esc => app.input = None,
            KeyCode::Enter => {
                if let Some((kind, text)) = app.input.take() {
                    app.submit_input(kind, text);
                }
            }
            KeyCode::Backspace => {
                if let Some((_, buf)) = app.input.as_mut() {
                    buf.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some((_, buf)) = app.input.as_mut() {
                    buf.push(c);
                }
            }
            _ => {}
        }
        return;
    }

    // 2) Chat focus (Board): ALL keys go to `claude`. jaum commands via the
    //    Ctrl+G prefix: Ctrl+G 1-2 tab · q quit · f finish · x remove · z
    //    fullscreen · n/p card · h back to cards · g sends literal Ctrl+G.
    if app.tab == Tab::Board && app.board_focus == BoardFocus::Chat && app.selected_card_is_live() {
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

    // Ctrl+C quits (in raw mode it doesn't become SIGINT; handle it here)
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    // 3) navigation (list focus: Tasks/Cards on the Board, or Docs)
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => app.tab = app.tab.next(),
        KeyCode::Char(c @ '1'..='2') => {
            app.tab = Tab::from_index(c as usize - '1' as usize);
        }
        // j/k navigate WITHIN the focused panel; h/l move focus (Tasks↔Cards↔Chat).
        KeyCode::Char('j') | KeyCode::Down => match (app.tab, app.board_focus) {
            (Tab::Docs, _) => app.docs_next(),
            (Tab::Board, BoardFocus::Cards) => app.card_next(),
            (Tab::Board, _) => app.select_next(),
        },
        KeyCode::Char('k') | KeyCode::Up => match (app.tab, app.board_focus) {
            (Tab::Docs, _) => app.docs_prev(),
            (Tab::Board, BoardFocus::Cards) => app.card_prev(),
            (Tab::Board, _) => app.select_prev(),
        },
        // Shift+J/K scrolls the doc preview without opening the overlay.
        KeyCode::Char('J') if app.tab == Tab::Docs => app.doc_scroll_down(),
        KeyCode::Char('K') if app.tab == Tab::Docs => app.doc_scroll_up(),
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),
        KeyCode::Char('l') | KeyCode::Right => {
            if app.tab == Tab::Board {
                app.focus_right();
            } else {
                app.tab = app.tab.next();
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            if app.tab == Tab::Board {
                app.focus_left();
            } else {
                app.tab = app.tab.prev();
            }
        }
        KeyCode::Enter | KeyCode::Char('o') => {
            if app.tab == Tab::Docs {
                app.open_doc();
            } else if app.board_focus == BoardFocus::Cards && app.selected_card_is_live() {
                app.board_focus = BoardFocus::Chat;
            } else {
                app.open_detail();
            }
        }
        KeyCode::Char('z') => app.chat_fullscreen = !app.chat_fullscreen,
        KeyCode::Char('p') => app.play_selected(),
        KeyCode::Char('r') => app.start_review_job(),
        KeyCode::Char('R') => app.review_selected(),
        KeyCode::Char('H') => app.handoff_selected(),
        KeyCode::Char('f') => app.finish_selected(),
        KeyCode::Char('i') => app.start_ingest_job(),
        KeyCode::Char('I') => app.start_init_input(),
        KeyCode::Char('a') => app.start_parallel_job(),
        KeyCode::Char('S') => app.setup_start(),
        KeyCode::Char('P') => app.open_picker(),
        KeyCode::Char('e') => app.request_edit_conventions(),
        // quick capture
        KeyCode::Char('c') => app.start_input(InputKind::Convention),
        KeyCode::Char('n') => app.start_input(InputKind::NewTask),
        KeyCode::Char('N') => app.start_input(InputKind::NewTaskClaude),
        KeyCode::Char('d') if app.selected_task().is_some() => app.start_input(InputKind::Defer),
        _ => {}
    }

    // Action commands re-show the toast (retry feedback): retrying a command
    // that fails the same way shows the error again instead of going silent.
    if matches!(
        key.code,
        KeyCode::Char('p' | 'r' | 'R' | 'H' | 'f' | 'i' | 'I' | 'S' | 'a')
    ) {
        app.rearm_toast();
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

pub(crate) fn render(f: &mut Frame, app: &App) {
    let (header, body, footer) = root_layout(f.area());

    render_header(f, app, header);
    match app.tab {
        Tab::Board => render_board(f, app, body),
        Tab::Docs => render_docs(f, app, body),
    }
    render_statusline(f, app, footer);

    if app.detail_open {
        render_detail(f, app);
    }
    if app.doc_open {
        render_doc_view(f, app);
    }
    if app.project_picker {
        render_picker(f, app);
    }
    if app.job_overlay {
        render_job(f, app);
    }
    // snackbar por cima de tudo
    if let Some(msg) = app.active_toast() {
        render_toast(f, msg);
    }
}

/// Live log overlay for a job (ingest/capture/init).
fn render_job(f: &mut Frame, app: &App) {
    use ratatui::widgets::Clear;
    let Some(job) = &app.job else { return };

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

/// Color style per status (board and badges).
fn status_color(s: jaum_core::Status) -> Color {
    use jaum_core::Status;
    match s {
        Status::Wip => Color::Green,
        Status::Review => Color::Yellow,
        Status::Ready => Color::Blue,
        Status::Backlog => Color::Gray,
        Status::Merged => Color::DarkGray,
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
fn render_doc_view(f: &mut Frame, app: &App) {
    use ratatui::widgets::Clear;

    let area = centered_rect(85, 85, f.area());
    f.render_widget(Clear, area);

    let title = app.docs.get(app.docs_selected).cloned().unwrap_or_default();
    // read fresh each frame: reflects external edits (setup) without close/reopen.
    let content = app
        .docs
        .get(app.docs_selected)
        .map(|rel| std::fs::read_to_string(app.docs_dir.join(rel)).unwrap_or_default())
        .unwrap_or_default();
    let p = Paragraph::new(markdown_lines(&content, area.width.saturating_sub(7)))
        .block(overlay_panel(&format!("{title} · j/k scroll · Esc close")))
        .wrap(Wrap { trim: false })
        .scroll((app.doc_scroll, 0));
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

/// Content lines of a task (metadata + markdown body). Reused by the Board
/// preview and the detail overlay.
fn task_detail_lines(t: &jaum_core::Task, width: u16) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:?}", t.task_type),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            status_label(t.status).to_uppercase(),
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
                jaum_core::Enforce::Hook => ("hook", Color::Red),
                jaum_core::Enforce::Review => ("review", PINK),
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

/// Overlay with the full content of the selected task (Enter/`o`).
fn render_detail(f: &mut Frame, app: &App) {
    use ratatui::widgets::Clear;
    let Some(t) = app.selected_task() else { return };
    let area = centered_rect(80, 80, f.area());
    f.render_widget(Clear, area);
    let p = Paragraph::new(task_detail_lines(t, area.width.saturating_sub(7)))
        .block(overlay_panel(&format!("{} · j/k scroll · Esc close", t.id)))
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    f.render_widget(p, area);
}

/// Header: logo + tab pills (left) and project name (right).
fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(app.project_name().chars().count() as u16 + 4),
    ])
    .split(area);

    let mut spans = vec![
        Span::styled(
            "◆ jaum",
            Style::default().fg(PINK).add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
    ];
    for (i, t) in Tab::all().iter().enumerate() {
        let label = format!(" {} {} ", i + 1, t.title());
        if *t == app.tab {
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
            format!(" {} ", app.project_name()),
            Style::default().fg(SUBTLE).add_modifier(Modifier::BOLD),
        ),
    ])
    .right_aligned();
    f.render_widget(Paragraph::new(right), cols[1]);
}

/// Central overlay with the project list (`P` key).
fn render_picker(f: &mut Frame, app: &App) {
    use ratatui::widgets::Clear;

    let area = centered_rect(60, 50, f.area());
    f.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .config
        .projects
        .iter()
        .map(|p| ListItem::new(format!("{}  {}", p.name, p.backlog.display())))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.picker_selected));
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

fn render_board(f: &mut Frame, app: &App, area: Rect) {
    // chat fullscreen: the card content takes the whole area.
    if app.chat_fullscreen {
        render_card_content(f, app, area);
        return;
    }
    let cols = board_layout(area);
    render_board_list(f, app, cols[0]);
    render_task_cards(f, app, cols[1]);
    render_card_content(f, app, cols[2]);
}

fn render_board_list(f: &mut Frame, app: &App, area: Rect) {
    use jaum_core::Status;
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_task: Vec<Option<usize>> = Vec::new();
    let mut last_status: Option<Status> = None;
    let active = app.active_task_ids();

    // synthetic "· project" row (top): holds the setup sessions.
    let mut proj = vec![Span::styled(
        "· project",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if app
        .sessions
        .iter()
        .any(|e| e.is_live() && e.kind == SessionKind::Setup)
    {
        proj.push(Span::styled(" ●", Style::default().fg(Color::Green)));
    }
    if app.setup_needed() {
        proj.push(Span::styled(
            " setup (S)",
            Style::default().fg(Color::Yellow),
        ));
    }
    items.push(ListItem::new(Line::from(proj)));
    row_to_task.push(None);

    for (i, t) in app.tasks.iter().enumerate() {
        if last_status != Some(t.status) {
            let count = app.tasks.iter().filter(|x| x.status == t.status).count();
            let color = status_color(t.status);
            items.push(ListItem::new(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(color)),
                Span::styled(
                    status_label(t.status).to_uppercase(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {count}"), Style::default().fg(Color::DarkGray)),
            ])));
            row_to_task.push(None);
            last_status = Some(t.status);
        }

        let badge = match t.status {
            Status::Wip => ("▶", Color::Green),
            Status::Merged => ("✔", Color::DarkGray),
            _ => ("·", SUBTLE),
        };
        let mut spans = vec![
            Span::styled(format!("{} ", badge.0), Style::default().fg(badge.1)),
            Span::styled(t.id.clone(), Style::default().add_modifier(Modifier::BOLD)),
        ];
        // live session open on this task (there's a chat to enter).
        if app
            .sessions
            .iter()
            .any(|e| e.is_live() && e.task.as_deref() == Some(t.id.as_str()))
        {
            spans.push(Span::styled(" ●", Style::default().fg(Color::Green)));
        }
        // review verdict (if there's a `.review.md`).
        if let Some(n) = app.review_badge(&t.id) {
            let (g, c) = if n == 0 {
                ("✓", Color::Green)
            } else {
                ("⚑", Color::Red)
            };
            spans.push(Span::styled(format!(" {g}"), Style::default().fg(c)));
        }
        // parallelism glyph (only when there are active tasks and this isn't one).
        if !active.contains(&t.id) {
            if app.parallel_conflict_with_active(&t.id).is_some() {
                spans.push(Span::styled(" ⚠", Style::default().fg(Color::Yellow)));
            } else if app.is_parallel_safe(&t.id) {
                spans.push(Span::styled(" ‖", Style::default().fg(Color::Green)));
            }
        }
        items.push(ListItem::new(Line::from(spans)));
        row_to_task.push(Some(i));
    }

    if items.is_empty() {
        items.push(ListItem::new(Span::styled(
            "empty — `i` to ingest the docs",
            Style::default().fg(SUBTLE),
        )));
    }

    let selected_row = if app.project_selected {
        Some(0) // the · project row is the first item
    } else {
        row_to_task.iter().position(|r| *r == Some(app.selected))
    };
    let mut state = ListState::default();
    state.select(selected_row);

    let title = format!("Board · {}", app.tasks.len());
    let list = List::new(items)
        .block(panel_focus(&title, app.board_focus == BoardFocus::Tasks))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Middle column: compact task detail at the top + the card list (sessions
/// + verdict). Cards are the navigable rows (`card_selected`).
fn render_task_cards(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.board_focus == BoardFocus::Cards;
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_card: Vec<Option<usize>> = Vec::new();
    // task body (goal/description), rendered below the Items.
    let mut body_lines: Vec<Line<'static>> = Vec::new();
    let mut detail = |items: &mut Vec<ListItem>, l: Line<'static>| {
        items.push(ListItem::new(l));
        row_to_card.push(None);
    };

    // compact detail (· project row or task)
    let title = if app.project_selected {
        detail(
            &mut items,
            Line::from(Span::styled(
                "project config (setup)",
                Style::default().fg(SUBTLE),
            )),
        );
        if app.setup_needed() {
            detail(
                &mut items,
                Line::from(Span::styled(
                    "setup pending — S opens the chat",
                    Style::default().fg(Color::Yellow),
                )),
            );
        }
        "· project".to_string()
    } else if let Some(t) = app.selected_task() {
        detail(
            &mut items,
            Line::from(Span::styled(
                format!("{:?} · {}", t.task_type, status_label(t.status)),
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
        if let Some(r) = app.load_review(&t.id) {
            let (txt, c) = if r.is_clean() {
                ("review CLEAN".to_string(), Color::Green)
            } else {
                (
                    format!("review DIRTY · {} pending", r.unmet_count()),
                    Color::Red,
                )
            };
            detail(
                &mut items,
                Line::from(Span::styled(txt, Style::default().fg(c))),
            );
        }
        if app.parallel_conflict_with_active(&t.id).is_some() {
            detail(
                &mut items,
                Line::from(Span::styled(
                    "⚠ parallel conflict",
                    Style::default().fg(Color::Yellow),
                )),
            );
        } else if app.is_parallel_safe(&t.id) {
            detail(
                &mut items,
                Line::from(Span::styled(
                    "‖ parallel ok",
                    Style::default().fg(Color::Green),
                )),
            );
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
    let cards = app.task_cards();
    if cards.is_empty() {
        detail(
            &mut items,
            Line::from(Span::styled(
                "  (none) — p play · R review · r verdict",
                Style::default().fg(SUBTLE),
            )),
        );
    } else {
        for (ci, card) in cards.iter().enumerate() {
            items.push(card_item(app, *card));
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
        .position(|r| *r == Some(app.card_selected));
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
fn card_item(app: &App, card: BoardCard) -> ListItem<'static> {
    match card {
        BoardCard::Session(i) => {
            let Some(e) = app.sessions.get(i) else {
                return ListItem::new(Line::from("  ?"));
            };
            let (dot, color) = if e.is_live() {
                ("●", Color::Green)
            } else {
                ("✓", Color::DarkGray)
            };
            let age = if e.is_live() {
                format!("active · {}", fmt_dur(age_of(e.last_activity)))
            } else {
                "closed".to_string()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(
                    e.kind.label().to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {age}"), Style::default().fg(SUBTLE)),
            ]))
        }
        BoardCard::Verdict => {
            let clean = app
                .selected_task()
                .and_then(|t| app.load_review(&t.id))
                .map(|r| r.is_clean())
                .unwrap_or(true);
            let (dot, color, txt) = if clean {
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
fn render_card_content(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.board_focus == BoardFocus::Chat || app.chat_fullscreen;
    match app.selected_card() {
        Some(BoardCard::Session(i)) => render_pty(f, app, i, area, focused),
        Some(BoardCard::Verdict) => {
            let id = app.selected_task().map(|t| t.id.clone());
            let lines = verdict_lines(app, id.as_deref());
            let title = match &id {
                Some(i) => format!("Verdict · {i}"),
                None => "Verdict".to_string(),
            };
            let p = Paragraph::new(lines)
                .block(panel_focus(&title, focused))
                .wrap(Wrap { trim: true });
            f.render_widget(p, area);
        }
        None => {
            // no card: task detail (or hint).
            let (title, lines) = match app.selected_task() {
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

/// Renders the PTY of session index `i` (or the history message).
fn render_pty(f: &mut Frame, app: &App, i: usize, area: Rect, focused: bool) {
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
    match app.sessions.get(i) {
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
fn verdict_lines(app: &App, id: Option<&str>) -> Vec<Line<'static>> {
    let report = id.and_then(|i| app.load_review(i));
    let mut lines: Vec<Line> = Vec::new();
    let Some(r) = report else {
        lines.push(Line::from(
            "No review yet. `r` runs the verdict (writes the report); `R` opens the review chat.",
        ));
        return lines;
    };
    let clean = r.is_clean();
    lines.push(Line::from(vec![
        Span::raw("is_clean: "),
        if clean {
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
            r.blocking_count(),
            r.findings.len()
        ),
        Style::default().fg(SUBTLE),
    )));
    if !clean {
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
            lines.push(Line::from(format!("  {}", finding.render())));
        }
    }
    push_verdict_section(&mut lines, "Constraints (enforce: review)", &r.constraints);
    push_verdict_section(&mut lines, "Acceptance criteria", &r.criteria);
    lines
}

/// Age of a wall-clock instant (time since `t`). Tolerant of a clock that went
/// backwards (clamped to zero).
fn age_of(t: std::time::SystemTime) -> std::time::Duration {
    std::time::SystemTime::now()
        .duration_since(t)
        .unwrap_or_default()
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
/// colored verdict, in the Review tab detail.
fn push_verdict_section(lines: &mut Vec<Line>, title: &str, items: &[ConstraintResult]) {
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
            ConstraintVerdict::Ok => ("OK", Color::Green),
            ConstraintVerdict::Failed => ("FAILED", Color::Red),
            ConstraintVerdict::Pending => ("PENDING", Color::Yellow),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  [{tag}] "), Style::default().fg(color)),
            Span::raw(c.text.clone()),
        ]));
    }
}

fn render_docs(f: &mut Frame, app: &App, area: Rect) {
    if app.docs.is_empty() {
        let p = Paragraph::new(format!(
            "(no .md in {})\n\nWrite the design docs here; then press `i` to ingest.",
            app.docs_dir.display()
        ))
        .block(panel("Docs"))
        .wrap(Wrap { trim: true });
        f.render_widget(p, area);
        return;
    }
    let cols = Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)])
        .spacing(1)
        .split(area);
    render_docs_list(f, app, cols[0]);
    render_docs_preview(f, app, cols[1]);
}

fn render_docs_list(f: &mut Frame, app: &App, area: Rect) {
    let group_of = |rel: &str| -> String {
        rel.split_once('/')
            .map(|(g, _)| g.to_string())
            .unwrap_or_default()
    };
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_doc: Vec<Option<usize>> = Vec::new();
    let mut last_group: Option<String> = None;

    for (i, rel) in app.docs.iter().enumerate() {
        let group = group_of(rel);
        if last_group.as_deref() != Some(group.as_str()) {
            let label = if group.is_empty() {
                "(root)".to_string()
            } else {
                group.to_uppercase()
            };
            let count = app.docs.iter().filter(|d| group_of(d) == group).count();
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

    let selected_row = row_to_doc
        .iter()
        .position(|r| *r == Some(app.docs_selected));
    let mut state = ListState::default();
    state.select(selected_row);

    let title = format!("Docs · {}", app.docs.len());
    let list = List::new(items)
        .block(panel(&title))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Live preview (rendered markdown) of the selected doc.
fn render_docs_preview(f: &mut Frame, app: &App, area: Rect) {
    let (title, lines) = match app.docs.get(app.docs_selected) {
        Some(rel) => {
            let file = rel.rsplit('/').next().unwrap_or(rel).to_string();
            let content = std::fs::read_to_string(app.docs_dir.join(rel)).unwrap_or_default();
            (
                format!("{file} · J/K scroll · Enter expand"),
                markdown_lines(&content, area.width.saturating_sub(7)),
            )
        }
        None => ("preview".to_string(), Vec::new()),
    };
    let p = Paragraph::new(lines)
        .block(panel(&title))
        .wrap(Wrap { trim: false })
        .scroll((app.doc_scroll, 0));
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

fn render_statusline(f: &mut Frame, app: &App, area: Rect) {
    // input mode: active prompt
    if let Some((kind, buf)) = &app.input {
        let label = match kind {
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
            Span::raw(format!(" {buf}")),
            Span::styled("█", Style::default().fg(PINK)),
            Span::styled("   Enter confirm · Esc cancel", Style::default().fg(SUBTLE)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    // statusline (left) + key caps (right)
    let keys = [
        ("p", "play"),
        ("r", "review"),
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
        Paragraph::new(Span::styled(app.statusline(), Style::default().fg(SUBTLE))),
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
