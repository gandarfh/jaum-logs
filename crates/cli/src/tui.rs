//! Render (ratatui) e event loop (crossterm) da TUI.

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

use crate::app::{App, InputKind, Tab, status_label};
use jaum_flows::review::ConstraintVerdict;

// --- tema (estética Charm/Lipgloss) ---------------------------------------
const ACCENT: Color = Color::Rgb(180, 142, 255); // lavanda (assinatura Charm)
const PINK: Color = Color::Rgb(255, 121, 198); // rosa de destaque
const BORDER: Color = Color::Rgb(82, 80, 112); // borda discreta
const SUBTLE: Color = Color::Rgb(128, 128, 150); // texto esmaecido
const SEL_BG: Color = Color::Rgb(54, 48, 92); // fundo da seleção
const BG_TITLE_FG: Color = Color::Rgb(20, 20, 28);

/// Painel base no estilo Charm: borda arredondada, título em "chip" e padding.
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

/// Painel sem padding (para o PTY preencher a área exata).
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

/// Estilo da linha selecionada (barra de fundo + texto forte).
fn sel_style() -> Style {
    Style::default()
        .bg(SEL_BG)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

const MARGIN_X: u16 = 2;
const MARGIN_Y: u16 = 1;

/// Geometria central: margem externa (gutter) + header/corpo/footer com folga.
/// Usado pelo render E pelo `sync_pty_size` para concordarem no tamanho do PTY.
fn root_layout(area: Rect) -> (Rect, Rect, Rect) {
    let inner = area.inner(Margin {
        vertical: MARGIN_Y,
        horizontal: MARGIN_X,
    });
    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // folga
        Constraint::Min(0),    // corpo
        Constraint::Length(1), // folga
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

/// Suspende a TUI, abre o `conventions.md` no `$EDITOR` e retoma, recarregando.
fn edit_conventions(terminal: &mut DefaultTerminal, app: &mut App) {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    ratatui::restore();
    let _ = std::process::Command::new(editor)
        .arg(&app.conventions_path)
        .status();
    *terminal = ratatui::init();
    let _ = terminal.clear();
    app.reload_conventions();
    app.status_msg = "conventions.md atualizado".into();
}

/// Mantém o PTY e o parser vt100 no tamanho EXATO do pane Session (corpo menos
/// as bordas). Sem isso, o `claude` quebra linha numa largura diferente da
/// renderizada e o texto sai corrompido.
fn sync_pty_size(terminal: &DefaultTerminal, app: &mut App) {
    let Ok(size) = terminal.size() else { return };
    sync_pty_to(app, size.width, size.height);
}

/// Como `sync_pty_size`, mas a partir de um tamanho explícito (usado pelo daemon,
/// que renderiza num backend em memória sem terminal real). Sincroniza só a
/// sessão SELECIONADA (a exibida) com o pane do terminal da master-detail.
pub(crate) fn sync_pty_to(app: &mut App, width: u16, height: u16) {
    let (_h, body, _f) = root_layout(Rect::new(0, 0, width, height));
    let (_list, term) = session_layout(body);
    // o pane do terminal tem borda (-2 em cada eixo)
    let cols = term.width.saturating_sub(2);
    let rows = term.height.saturating_sub(2);
    if cols == 0 || rows == 0 {
        return;
    }
    let sel = app.session_selected;
    if let Some(e) = app.sessions.get_mut(sel)
        && e.parser.screen().size() != (rows, cols)
    {
        if let Some(s) = &e.session {
            let _ = s.resize(rows, cols);
        }
        e.parser.screen_mut().set_size(rows, cols);
    }
}

/// Área interior (sem borda) do pane do terminal da sessão, em coords absolutas.
pub(crate) fn session_term_area(width: u16, height: u16) -> Rect {
    let (_h, body, _f) = root_layout(Rect::new(0, 0, width, height));
    let (_list, term) = session_layout(body);
    Rect {
        x: term.x + 1,
        y: term.y + 1,
        width: term.width.saturating_sub(2),
        height: term.height.saturating_sub(2),
    }
}

/// Trata um evento de mouse sobre a aba Session: se o claude está em modo mouse
/// (SGR), encaminha o evento pro PTY; senão, rola a scrollback do terminal
/// embutido (vt100) com a roda.
pub(crate) fn handle_mouse(app: &mut App, ev: crossterm::event::MouseEvent, width: u16, height: u16) {
    use crossterm::event::MouseEventKind;
    if app.tab != Tab::Session {
        return;
    }
    let area = session_term_area(width, height);
    let inside = ev.column >= area.x
        && ev.column < area.x.saturating_add(area.width)
        && ev.row >= area.y
        && ev.row < area.y.saturating_add(area.height);
    if !inside {
        return;
    }
    let sel = app.session_selected;
    let Some(e) = app.sessions.get_mut(sel) else {
        return;
    };

    let screen = e.parser.screen();
    let mouse_on = screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
        && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr;

    if mouse_on {
        let col = ev.column - area.x + 1; // 1-based, relativo ao PTY
        let row = ev.row - area.y + 1;
        if let Some(bytes) = encode_mouse_sgr(&ev, col, row)
            && let Some(s) = &mut e.session
        {
            let _ = s.write_input(&bytes);
        }
        return;
    }

    // claude não capturou o mouse: roda a scrollback do terminal embutido.
    let back = e.parser.screen().scrollback();
    match ev.kind {
        MouseEventKind::ScrollUp => e.parser.screen_mut().set_scrollback(back + 3),
        MouseEventKind::ScrollDown => e.parser.screen_mut().set_scrollback(back.saturating_sub(3)),
        _ => {}
    }
}

/// Codifica um evento de mouse no protocolo SGR (1006): `ESC [ < cb ; col ; row (M|m)`.
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
    // 0) overlay de logs de job (ingest/captura/init)
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

    // 0) picker de projeto (overlay)
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

    // 0.5) overlay de detalhe da task
    if app.detail_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.close_detail(),
            KeyCode::Char('j') | KeyCode::Down => app.detail_scroll_down(),
            KeyCode::Char('k') | KeyCode::Up => app.detail_scroll_up(),
            _ => {}
        }
        return;
    }

    // 0.6) overlay de visualização de doc (markdown)
    if app.doc_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.close_doc(),
            KeyCode::Char('j') | KeyCode::Down => app.doc_scroll_down(),
            KeyCode::Char('k') | KeyCode::Up => app.doc_scroll_up(),
            _ => {}
        }
        return;
    }

    // 1) captura de texto (defer / convenção / nova task)
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

    // 2) aba Session com sessão viva: TODAS as teclas vão para o `claude`.
    //    Para comandos do jaum, use o prefixo Ctrl+G (Ctrl+B é do tmux):
    //      Ctrl+G 1-4 troca aba · Ctrl+G q detach · Ctrl+G x encerra sessão
    //      Ctrl+G n/p (ou j/k) troca de sessão · Ctrl+G g envia Ctrl+G
    if app.tab == Tab::Session && !app.sessions.is_empty() {
        if app.pending_prefix {
            app.pending_prefix = false;
            match key.code {
                KeyCode::Char(c @ '1'..='4') => {
                    app.tab = Tab::from_index(c as usize - '1' as usize)
                }
                KeyCode::Char('q') => app.should_quit = true,
                KeyCode::Char('f') => app.finish_selected_session(),
                KeyCode::Char('x') => app.close_selected_session(),
                KeyCode::Char('n') | KeyCode::Char('j') => app.session_next(),
                KeyCode::Char('p') | KeyCode::Char('k') => app.session_prev(),
                KeyCode::Char('g') => {
                    let sel = app.session_selected;
                    if let Some(e) = app.sessions.get_mut(sel)
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
        let sel = app.session_selected;
        if let Some(e) = app.sessions.get_mut(sel) {
            let bytes = key_to_bytes(key);
            if !bytes.is_empty()
                && let Some(s) = &mut e.session
            {
                let _ = s.write_input(&bytes);
            }
        }
        return;
    }

    // Ctrl+C sai (em raw mode não vira SIGINT; precisa ser tratado aqui)
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    // 3) atalhos globais (NAV) — movimentação estilo vim
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => app.tab = app.tab.next(),
        KeyCode::Char(c @ '1'..='4') => {
            app.tab = Tab::from_index(c as usize - '1' as usize);
        }
        // vim: j/k cima-baixo (ciente da aba), h/l troca de aba
        KeyCode::Char('j') | KeyCode::Down => match app.tab {
            Tab::Docs => app.docs_next(),
            Tab::Review => app.review_next(),
            _ => app.select_next(),
        },
        KeyCode::Char('k') | KeyCode::Up => match app.tab {
            Tab::Docs => app.docs_prev(),
            Tab::Review => app.review_prev(),
            _ => app.select_prev(),
        },
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),
        KeyCode::Char('l') | KeyCode::Right => app.tab = app.tab.next(),
        KeyCode::Char('h') | KeyCode::Left => app.tab = app.tab.prev(),
        KeyCode::Enter | KeyCode::Char('o') => {
            if app.tab == Tab::Docs {
                app.open_doc()
            } else {
                app.open_detail()
            }
        }
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
        // captura rápida
        KeyCode::Char('c') => app.start_input(InputKind::Convention),
        KeyCode::Char('n') => app.start_input(InputKind::NewTask),
        KeyCode::Char('N') => app.start_input(InputKind::NewTaskClaude),
        KeyCode::Char('d') if app.selected_task().is_some() => app.start_input(InputKind::Defer),
        _ => {}
    }

    // Comandos de ação re-mostram o toast (feedback no retry): re-tentar um
    // comando que falha igual exibe o erro de novo, em vez de silêncio.
    if matches!(
        key.code,
        KeyCode::Char('p' | 'r' | 'R' | 'H' | 'f' | 'i' | 'I' | 'S' | 'a')
    ) {
        app.rearm_toast();
    }
}

/// Traduz uma tecla para bytes do PTY (cobertura básica).
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
        Tab::Session => render_session(f, app, body),
        Tab::Review => render_review(f, app, body),
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

/// Overlay de logs ao vivo de um job (ingest/captura/init).
fn render_job(f: &mut Frame, app: &App) {
    use ratatui::widgets::Clear;
    let Some(job) = &app.job else { return };

    let area = centered_rect(82, 72, f.area());
    f.render_widget(Clear, area);

    let (state_txt, color) = if job.finished {
        ("concluído", Color::Green)
    } else {
        ("rodando…", ACCENT)
    };
    // dica de navegação: muda conforme está ao vivo ou travado lendo.
    let nav = if job.follow {
        "j/k rola · Esc fecha"
    } else {
        "j/k rola · G ao vivo · Esc fecha"
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

    // área interna: bordas (-2) + padding horizontal (2+2) e padding superior (1).
    let inner_w = area.width.saturating_sub(6).max(1) as usize;
    let visible_rows = area.height.saturating_sub(3).max(1) as usize;

    let lines: Vec<Line> = job.logs.iter().map(|l| job_log_line(l)).collect();

    // estima a altura com wrap para clampar o scroll e fazer o follow.
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

/// Estiliza uma linha de log do job conforme seu prefixo.
fn job_log_line(l: &str) -> Line<'static> {
    if let Some(rest) = l.strip_prefix("→ ") {
        // "Tool argumento…" — destaca o nome da tool.
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
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ))
    } else if let Some(rest) = l.strip_prefix('[') {
        // "[modelo] texto…" — modelo esmaecido, texto claro.
        match rest.split_once("] ") {
            Some((model, text)) => Line::from(vec![
                Span::styled(format!("[{model}] "), Style::default().fg(ACCENT)),
                Span::styled(text.to_string(), Style::default().fg(Color::White)),
            ]),
            None => Line::from(Span::styled(l.to_string(), Style::default().fg(Color::White))),
        }
    } else {
        Line::from(Span::styled(l.to_string(), Style::default().fg(SUBTLE)))
    }
}

/// Estilo de cor por status (board e badges).
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

/// Render simples de markdown -> linhas estilizadas (headings, listas,
/// checklists, code fences, blockquotes).
fn markdown_lines(text: &str) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        if let Some(lang) = raw.trim_start().strip_prefix("```") {
            in_code = !in_code;
            let bar = if in_code {
                format!("┌─ {}", lang.trim())
            } else {
                "└─".to_string()
            };
            out.push(Line::from(Span::styled(bar, Style::default().fg(Color::DarkGray))));
            continue;
        }
        if in_code {
            out.push(Line::from(Span::styled(
                format!("  {raw}"),
                Style::default().fg(Color::Rgb(170, 170, 200)),
            )));
            continue;
        }
        let bold = Modifier::BOLD;
        if let Some(h) = raw.strip_prefix("### ") {
            out.push(Line::from(Span::styled(
                h.to_string(),
                Style::default().fg(Color::Cyan).add_modifier(bold),
            )));
        } else if let Some(h) = raw.strip_prefix("## ") {
            out.push(Line::from(Span::styled(
                h.to_string(),
                Style::default().fg(Color::Magenta).add_modifier(bold),
            )));
        } else if let Some(h) = raw.strip_prefix("# ") {
            out.push(Line::from(Span::styled(
                h.to_uppercase(),
                Style::default().fg(Color::LightMagenta).add_modifier(bold),
            )));
        } else if raw.starts_with("- [ ] ") || raw.starts_with("- [x] ") {
            let done = raw.starts_with("- [x] ");
            let mark = if done { "☑" } else { "☐" };
            let txt = raw[6..].to_string();
            out.push(Line::from(vec![
                Span::styled(
                    format!("  {mark} "),
                    Style::default().fg(if done { Color::Green } else { Color::Yellow }),
                ),
                Span::raw(txt),
            ]));
        } else if let Some(item) = raw.strip_prefix("- ").or_else(|| raw.strip_prefix("* ")) {
            out.push(Line::from(format!("  • {item}")));
        } else if let Some(q) = raw.strip_prefix("> ") {
            out.push(Line::from(Span::styled(
                format!("┃ {q}"),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            )));
        } else {
            out.push(Line::from(raw.to_string()));
        }
    }
    out
}

/// Overlay de visualização de um doc (markdown renderizado).
fn render_doc_view(f: &mut Frame, app: &App) {
    use ratatui::widgets::Clear;

    let area = centered_rect(85, 85, f.area());
    f.render_widget(Clear, area);

    let title = app
        .docs
        .get(app.docs_selected)
        .cloned()
        .unwrap_or_default();
    // lê fresh a cada frame: reflete edições externas (setup) sem fechar/reabrir.
    let content = app
        .docs
        .get(app.docs_selected)
        .map(|rel| std::fs::read_to_string(app.docs_dir.join(rel)).unwrap_or_default())
        .unwrap_or_default();
    let p = Paragraph::new(markdown_lines(&content))
        .block(overlay_panel(&format!("{title} · j/k rola · Esc fecha")))
        .wrap(Wrap { trim: false })
        .scroll((app.doc_scroll, 0));
    f.render_widget(p, area);
}

/// Painel de overlay: como `panel`, mas com borda de acento (foco).
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

/// Linhas do conteúdo de uma task (metadados + corpo em markdown). Reusado pelo
/// preview do Board e pelo overlay de detalhe.
fn task_detail_lines(t: &jaum_core::Task) -> Vec<Line<'static>> {
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
            "PR não criado".to_string()
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
    lines.extend(markdown_lines(&t.body));
    lines
}

/// Overlay com o conteúdo completo da task selecionada (Enter/`o`).
fn render_detail(f: &mut Frame, app: &App) {
    use ratatui::widgets::Clear;
    let Some(t) = app.selected_task() else { return };
    let area = centered_rect(80, 80, f.area());
    f.render_widget(Clear, area);
    let p = Paragraph::new(task_detail_lines(t))
        .block(overlay_panel(&format!("{} · j/k rola · Esc fecha", t.id)))
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    f.render_widget(p, area);
}

/// Header: logo + pills das abas (esquerda) e nome do projeto (direita).
fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(app.project_name().chars().count() as u16 + 4),
    ])
    .split(area);

    let mut spans = vec![
        Span::styled("◆ jaum", Style::default().fg(PINK).add_modifier(Modifier::BOLD)),
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

/// Overlay central com a lista de projetos (tecla `P`).
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
        .block(overlay_panel("Projects · Enter troca · Esc fecha"))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Retângulo centralizado (percentuais da área).
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

fn render_board(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
        .spacing(1)
        .split(area);
    render_board_list(f, app, cols[0]);
    render_board_preview(f, app, cols[1]);
}

fn render_board_list(f: &mut Frame, app: &App, area: Rect) {
    use jaum_core::Status;
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_task: Vec<Option<usize>> = Vec::new();
    let mut last_status: Option<Status> = None;
    let active = app.active_task_ids();

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
            Status::Review => app
                .review_badge(&t.id)
                .map(|n| if n == 0 { ("✓", Color::Green) } else { ("⚑", Color::Yellow) })
                .unwrap_or((" ", SUBTLE)),
            Status::Merged => ("✔", Color::DarkGray),
            _ => ("·", SUBTLE),
        };
        let mut spans = vec![
            Span::styled(format!("{} ", badge.0), Style::default().fg(badge.1)),
            Span::styled(t.id.clone(), Style::default().add_modifier(Modifier::BOLD)),
        ];
        // glifo de paralelismo (só quando há tasks ativas e esta não é uma delas).
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
            "vazio — `i` para ingerir os docs",
            Style::default().fg(SUBTLE),
        )));
    }

    let selected_row = row_to_task.iter().position(|r| *r == Some(app.selected));
    let mut state = ListState::default();
    state.select(selected_row);

    let mut title = format!("Board · {}", app.tasks.len());
    if app.setup_needed() {
        title.push_str(" · setup pendente (S)");
    }
    let list = List::new(items)
        .block(panel(&title))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Preview ao vivo da task selecionada (sem precisar abrir).
fn render_board_preview(f: &mut Frame, app: &App, area: Rect) {
    let (title, lines) = match app.selected_task() {
        Some(t) => {
            let mut lines = task_detail_lines(t);
            // paralelismo: conflito com uma task ativa, ou ok para paralelo.
            if let Some((other, repo, reason)) = app.parallel_conflict_with_active(&t.id) {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("⚠ conflita com ", Style::default().fg(Color::Yellow)),
                    Span::styled(other, Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" no repo {repo}"), Style::default().fg(SUBTLE)),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("  {reason}"),
                    Style::default().fg(SUBTLE),
                )));
            } else if app.is_parallel_safe(&t.id) {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "‖ pode rodar em paralelo com as ativas",
                    Style::default().fg(Color::Green),
                )));
            }
            (format!("{} · Enter expande", t.id), lines)
        }
        None => (
            "Detalhes".to_string(),
            vec![Line::from(Span::styled(
                "selecione uma task (j/k)",
                Style::default().fg(SUBTLE),
            ))],
        ),
    };
    let p = Paragraph::new(lines)
        .block(panel(&title))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Split da aba Session: lista de sessões (esquerda) + terminal (direita).
fn session_layout(area: Rect) -> (Rect, Rect) {
    let cols = Layout::horizontal([Constraint::Percentage(26), Constraint::Percentage(74)])
        .spacing(1)
        .split(area);
    (cols[0], cols[1])
}

/// Idade de um instante de parede (quanto tempo desde `t`). Tolerante a relógio
/// que andou para trás (clamp em zero).
fn age_of(t: std::time::SystemTime) -> std::time::Duration {
    std::time::SystemTime::now()
        .duration_since(t)
        .unwrap_or_default()
}

/// Duração curta e legível: `8s`, `3m`, `1h2m`.
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

fn render_session(f: &mut Frame, app: &App, area: Rect) {
    if app.sessions.is_empty() {
        let block = panel("Session");
        let p = Paragraph::new(
            "Sem sessões. No Board, tecle `p` (play), `r` (review) ou `S` (setup).\nVárias rodam em paralelo — troque com Ctrl+G n/p.",
        )
        .block(block)
        .wrap(Wrap { trim: true });
        f.render_widget(p, area);
        return;
    }
    let (left, right) = session_layout(area);
    render_session_list(f, app, left);
    render_session_term(f, app, right);
}

/// Lista das sessões: nome, task vinculada, criada há / status (rodando/encerrada).
fn render_session_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|e| {
            let (dot, color) = if e.finished {
                ("✓", Color::DarkGray)
            } else {
                ("●", Color::Green)
            };
            let head = Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(e.name(), Style::default().add_modifier(Modifier::BOLD)),
            ]);
            let status = if e.finished {
                format!("criada há {} · ✓ encerrada", fmt_dur(age_of(e.created)))
            } else {
                format!(
                    "criada há {} · ▶ saída há {}",
                    fmt_dur(age_of(e.created)),
                    fmt_dur(age_of(e.last_activity))
                )
            };
            let meta = Line::from(Span::styled(format!("  {status}"), Style::default().fg(SUBTLE)));
            ListItem::new(vec![head, meta])
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.session_selected));

    let title = format!("Sessões · {}", app.sessions.len());
    let list = List::new(items)
        .block(panel(&title))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Terminal da sessão selecionada.
fn render_session_term(f: &mut Frame, app: &App, area: Rect) {
    let hint = if app.pending_prefix {
        "Ctrl+G… (1-4 aba · f finaliza · x remove · n/p sessão · q detach · g=Ctrl+G)"
    } else {
        "digitando no claude · Ctrl+G = comando jaum"
    };
    let block = panel_tight(&format!("Terminal — {hint}"));
    match app.selected_session() {
        // sessão de histórico (sem PTY): não há terminal vivo para mostrar.
        Some(e) if !e.is_live() => {
            let msg = "Sessão encerrada (histórico). Sem terminal vivo.\nO contexto fica salvo no claude; abra um novo play/review para continuar.";
            f.render_widget(
                Paragraph::new(msg).style(Style::default().fg(SUBTLE)).block(block),
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

/// Aba Review: lista de reviews (esquerda) + detalhe do selecionado (direita).
fn render_review(f: &mut Frame, app: &App, area: Rect) {
    let (left, right) = session_layout(area);
    render_review_list(f, app, left);
    render_review_detail(f, app, right);
}

/// Lista das tasks que já têm review (`.review.md`): id + LIMPO/SUJO + nº bloqueantes.
fn render_review_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .review_ids
        .iter()
        .map(|id| {
            let report = app.load_review(id);
            let clean = report.as_ref().map(|r| r.is_clean()).unwrap_or(true);
            let blocking = report.as_ref().map(|r| r.blocking_count()).unwrap_or(0);
            let (dot, color) = if clean {
                ("✓", Color::Green)
            } else {
                ("⚑", Color::Red)
            };
            let head = Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(id.clone(), Style::default().add_modifier(Modifier::BOLD)),
            ]);
            let meta = Line::from(Span::styled(
                format!("  {} bloqueante(s)", blocking),
                Style::default().fg(SUBTLE),
            ));
            ListItem::new(vec![head, meta])
        })
        .collect();

    let title = format!("Reviews · {}", app.review_ids.len());
    if items.is_empty() {
        let p = Paragraph::new("nenhum review ainda\nrode `r` numa task")
            .style(Style::default().fg(SUBTLE))
            .block(panel(&title));
        f.render_widget(p, area);
        return;
    }
    let mut state = ListState::default();
    state.select(Some(app.review_selected.min(app.review_ids.len().saturating_sub(1))));
    let list = List::new(items)
        .block(panel(&title))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Detalhe do review sob o cursor da lista.
fn render_review_detail(f: &mut Frame, app: &App, area: Rect) {
    let id = app.review_tab_task();
    let report = id.as_deref().and_then(|i| app.load_review(i));

    let mut lines: Vec<Line> = Vec::new();
    match report {
        None => lines.push(Line::from(
            "Sem review ainda. `r` roda o veredicto (read-only, grava o report); `R` abre o chat de review.",
        )),
        Some(r) => {
            let clean = r.is_clean();
            lines.push(Line::from(vec![
                Span::raw("is_clean: "),
                if clean {
                    Span::styled(
                        "LIMPO",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled(
                        "SUJO",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )
                },
            ]));
            // resumo: bloqueantes (blocker/major) vs total; só os primeiros reprovam.
            let blocking = r.blocking_count();
            lines.push(Line::from(Span::styled(
                format!(
                    "{} bloqueante(s) (blocker/major) · {} finding(s) no total · minor/nit não reprovam",
                    blocking,
                    r.findings.len()
                ),
                Style::default().fg(SUBTLE),
            )));
            if !clean {
                lines.push(Line::from(Span::styled(
                    "`H` envia os findings para a sessão de play corrigir",
                    Style::default().fg(SUBTLE),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Findings",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            if r.findings.is_empty() {
                lines.push(Line::from("  (nenhum)"));
            } else {
                for finding in &r.findings {
                    lines.push(Line::from(format!("  {}", finding.render())));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Constraints (enforce: review)",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for c in &r.constraints {
                let (tag, color) = match c.verdict {
                    ConstraintVerdict::Ok => ("OK", Color::Green),
                    ConstraintVerdict::Reprovado => ("REPROVADO", Color::Red),
                    ConstraintVerdict::Pending => ("PENDENTE", Color::Yellow),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  [{tag}] "), Style::default().fg(color)),
                    Span::raw(c.text.clone()),
                ]));
            }
        }
    }
    let title = match id {
        Some(i) => format!("Review · {i}"),
        None => "Review".to_string(),
    };
    let p = Paragraph::new(lines)
        .block(panel(&title))
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn render_docs(f: &mut Frame, app: &App, area: Rect) {
    if app.docs.is_empty() {
        let p = Paragraph::new(format!(
            "(nenhum .md em {})\n\nEscreva os docs de design aqui; depois tecle `i` para o ingest.",
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
        rel.split_once('/').map(|(g, _)| g.to_string()).unwrap_or_default()
    };
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_doc: Vec<Option<usize>> = Vec::new();
    let mut last_group: Option<String> = None;

    for (i, rel) in app.docs.iter().enumerate() {
        let group = group_of(rel);
        if last_group.as_deref() != Some(group.as_str()) {
            let label = if group.is_empty() {
                "(raiz)".to_string()
            } else {
                group.to_uppercase()
            };
            let count = app.docs.iter().filter(|d| group_of(d) == group).count();
            let color = doc_group_color(&group);
            items.push(ListItem::new(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(color)),
                Span::styled(label, Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {count}"), Style::default().fg(Color::DarkGray)),
            ])));
            row_to_doc.push(None);
            last_group = Some(group);
        }
        let file = rel.rsplit('/').next().unwrap_or(rel);
        items.push(ListItem::new(Line::from(format!("  {file}"))));
        row_to_doc.push(Some(i));
    }

    let selected_row = row_to_doc.iter().position(|r| *r == Some(app.docs_selected));
    let mut state = ListState::default();
    state.select(selected_row);

    let title = format!("Docs · {}", app.docs.len());
    let list = List::new(items)
        .block(panel(&title))
        .highlight_style(sel_style())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Preview ao vivo (markdown renderizado) do doc selecionado.
fn render_docs_preview(f: &mut Frame, app: &App, area: Rect) {
    let (title, lines) = match app.docs.get(app.docs_selected) {
        Some(rel) => {
            let file = rel.rsplit('/').next().unwrap_or(rel).to_string();
            let content = std::fs::read_to_string(app.docs_dir.join(rel)).unwrap_or_default();
            (format!("{file} · Enter expande"), markdown_lines(&content))
        }
        None => ("preview".to_string(), Vec::new()),
    };
    let p = Paragraph::new(lines)
        .block(panel(&title))
        .wrap(Wrap { trim: false })
        .scroll((app.doc_scroll, 0));
    f.render_widget(p, area);
}

/// Cor por categoria de doc (só estética).
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
    // modo input: prompt ativo
    if let Some((kind, buf)) = &app.input {
        let label = match kind {
            InputKind::Defer => "defer",
            InputKind::Convention => "convenção",
            InputKind::NewTask => "nova task",
            InputKind::NewTaskClaude => "task (claude investiga)",
            InputKind::InitPath => "init (caminho do projeto)",
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
            Span::styled(
                "   Enter confirma · Esc cancela",
                Style::default().fg(SUBTLE),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    // statusline (esquerda) + key caps (direita)
    let keys = [
        ("p", "play"),
        ("r", "review"),
        ("f", "finish"),
        ("a", "paralelo"),
        ("S", "setup"),
        ("i", "ingest"),
        ("I", "init"),
        ("N", "claude"),
        ("q", "sair"),
    ];
    let mut hint: Vec<Span> = Vec::new();
    for (k, label) in keys {
        hint.push(Span::styled(
            format!(" {k}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        hint.push(Span::styled(format!(" {label} "), Style::default().fg(SUBTLE)));
    }

    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(84)]).split(area);
    f.render_widget(
        Paragraph::new(Span::styled(app.statusline(), Style::default().fg(SUBTLE))),
        cols[0],
    );
    f.render_widget(Paragraph::new(Line::from(hint)).right_aligned(), cols[1]);
}

/// Snackbar temporário no canto superior direito (feedback de interação).
fn render_toast(f: &mut Frame, msg: &str) {
    use ratatui::widgets::Clear;
    let area = f.area();
    let w = (msg.chars().count() as u16 + 7)
        .min(area.width.saturating_sub(2))
        .max(14);
    let x = area.right().saturating_sub(w + 1);
    let y = area.top() + 4; // abaixo da barra de tabs
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
    use super::encode_mouse_sgr;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn ev(kind: MouseEventKind) -> MouseEvent {
        MouseEvent { kind, column: 1, row: 1, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn encode_sgr_scroll_e_click() {
        // roda pra cima: botão 64, sufixo M
        assert_eq!(
            encode_mouse_sgr(&ev(MouseEventKind::ScrollUp), 5, 9).unwrap(),
            b"\x1b[<64;5;9M".to_vec()
        );
        // press do esquerdo: cb 0, M
        assert_eq!(
            encode_mouse_sgr(&ev(MouseEventKind::Down(MouseButton::Left)), 2, 3).unwrap(),
            b"\x1b[<0;2;3M".to_vec()
        );
        // release: mesmo cb, sufixo m
        assert_eq!(
            encode_mouse_sgr(&ev(MouseEventKind::Up(MouseButton::Left)), 2, 3).unwrap(),
            b"\x1b[<0;2;3m".to_vec()
        );
        // movimento puro não vira nada
        assert!(encode_mouse_sgr(&ev(MouseEventKind::Moved), 1, 1).is_none());
    }
}
