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

use crate::app::{App, BoardCard, BoardFocus, InputKind, SessionKind, Tab, status_label};
use jaum_flows::review::{ConstraintResult, ConstraintVerdict};

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
    // o chat é a 3ª coluna do Board (ou a área toda em fullscreen).
    let term = if app.chat_fullscreen { body } else { board_layout(body)[2] };
    // o pane do terminal tem borda (-2 em cada eixo)
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

/// Área interior (sem borda) do pane do chat, em coords absolutas.
pub(crate) fn session_term_area(app: &App, width: u16, height: u16) -> Rect {
    let (_h, body, _f) = root_layout(Rect::new(0, 0, width, height));
    let term = if app.chat_fullscreen { body } else { board_layout(body)[2] };
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

    // 2) foco no Chat (Board): TODAS as teclas vão para o `claude`. Comandos do
    //    jaum via prefixo Ctrl+G: Ctrl+G 1-2 aba · q sair · f finaliza · x remove
    //    · z fullscreen · n/p card · h volta pros cards · g envia Ctrl+G literal.
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
        // Esc sai do chat de volta pros cards (não é repassado ao PTY).
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

    // Ctrl+C sai (em raw mode não vira SIGINT; precisa ser tratado aqui)
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    // 3) navegação (foco em listas: Tasks/Cards no Board, ou Docs)
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => app.tab = app.tab.next(),
        KeyCode::Char(c @ '1'..='2') => {
            app.tab = Tab::from_index(c as usize - '1' as usize);
        }
        // j/k navegam DENTRO do painel em foco; h/l movem o foco (Tasks↔Cards↔Chat).
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

/// Split do Board: tasks | itens da task (cards) | chat/conteúdo.
pub(crate) fn board_layout(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::horizontal([
        Constraint::Percentage(24),
        Constraint::Percentage(28),
        Constraint::Percentage(48),
    ])
    .spacing(1)
    .split(area)
}

/// Bloco com borda de destaque quando o painel está em foco.
fn panel_focus(title: &str, focused: bool) -> Block<'static> {
    let b = panel(title);
    if focused {
        b.border_style(Style::default().fg(ACCENT))
    } else {
        b
    }
}

fn render_board(f: &mut Frame, app: &App, area: Rect) {
    // fullscreen do chat: o conteúdo do card ocupa a área toda.
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

    // linha sintética "· projeto" (topo): abriga as sessões de setup.
    let mut proj = vec![Span::styled(
        "· projeto",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if app.sessions.iter().any(|e| e.is_live() && e.kind == SessionKind::Setup) {
        proj.push(Span::styled(" ●", Style::default().fg(Color::Green)));
    }
    if app.setup_needed() {
        proj.push(Span::styled(" setup (S)", Style::default().fg(Color::Yellow)));
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
        // sessão viva aberta nesta task (há chat pra entrar).
        if app.sessions.iter().any(|e| e.is_live() && e.task.as_deref() == Some(t.id.as_str())) {
            spans.push(Span::styled(" ●", Style::default().fg(Color::Green)));
        }
        // veredito do review (se houver `.review.md`).
        if let Some(n) = app.review_badge(&t.id) {
            let (g, c) = if n == 0 { ("✓", Color::Green) } else { ("⚑", Color::Red) };
            spans.push(Span::styled(format!(" {g}"), Style::default().fg(c)));
        }
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

    let selected_row = if app.project_selected {
        Some(0) // a linha · projeto é o primeiro item
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

/// Coluna do meio: detalhe compacto da task no topo + a lista de cards (sessões
/// + veredito). Cards são as linhas navegáveis (`card_selected`).
fn render_task_cards(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.board_focus == BoardFocus::Cards;
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_card: Vec<Option<usize>> = Vec::new();
    // corpo da task (objetivo/descrição), renderizado abaixo dos Itens.
    let mut body_lines: Vec<Line<'static>> = Vec::new();
    let mut detail = |items: &mut Vec<ListItem>, l: Line<'static>| {
        items.push(ListItem::new(l));
        row_to_card.push(None);
    };

    // detalhe compacto (linha · projeto ou task)
    let title = if app.project_selected {
        detail(
            &mut items,
            Line::from(Span::styled("config do projeto (setup)", Style::default().fg(SUBTLE))),
        );
        if app.setup_needed() {
            detail(
                &mut items,
                Line::from(Span::styled(
                    "setup pendente — S abre o chat",
                    Style::default().fg(Color::Yellow),
                )),
            );
        }
        "· projeto".to_string()
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
                format!("sem PR · {}", pr.branch)
            };
            detail(&mut items, Line::from(Span::styled(pr_txt, Style::default().fg(SUBTLE))));
        }
        if let Some(r) = app.load_review(&t.id) {
            let (txt, c) = if r.is_clean() {
                ("review LIMPO".to_string(), Color::Green)
            } else {
                (format!("review SUJO · {} pend.", r.unmet_count()), Color::Red)
            };
            detail(&mut items, Line::from(Span::styled(txt, Style::default().fg(c))));
        }
        if app.parallel_conflict_with_active(&t.id).is_some() {
            detail(
                &mut items,
                Line::from(Span::styled("⚠ conflita em paralelo", Style::default().fg(Color::Yellow))),
            );
        } else if app.is_parallel_safe(&t.id) {
            detail(
                &mut items,
                Line::from(Span::styled("‖ paralelo ok", Style::default().fg(Color::Green))),
            );
        }
        if !t.body.trim().is_empty() {
            body_lines = markdown_lines(&t.body);
        }
        t.id.clone()
    } else {
        let p = Paragraph::new("selecione uma task (j/k)")
            .style(Style::default().fg(SUBTLE))
            .block(panel_focus("Task", focused));
        f.render_widget(p, area);
        return;
    };

    // separador + cards
    detail(&mut items, Line::from(""));
    detail(
        &mut items,
        Line::from(Span::styled("Itens", Style::default().add_modifier(Modifier::BOLD))),
    );
    let cards = app.task_cards();
    if cards.is_empty() {
        detail(
            &mut items,
            Line::from(Span::styled(
                "  (nenhum) — p play · R review · r veredito",
                Style::default().fg(SUBTLE),
            )),
        );
    } else {
        for (ci, card) in cards.iter().enumerate() {
            items.push(card_item(app, *card));
            row_to_card.push(Some(ci));
        }
    }

    // corpo da task (objetivo/descrição) abaixo dos Itens.
    if !body_lines.is_empty() {
        items.push(ListItem::new(Line::from("")));
        row_to_card.push(None);
        for l in body_lines {
            items.push(ListItem::new(l));
            row_to_card.push(None);
        }
    }

    let selected_row = row_to_card.iter().position(|r| *r == Some(app.card_selected));
    let mut state = ListState::default();
    state.select(selected_row);
    let hl = if focused { sel_style() } else { Style::default().add_modifier(Modifier::BOLD) };
    let list = List::new(items)
        .block(panel_focus(&title, focused))
        .highlight_style(hl)
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut state);
}

/// Uma linha de card na coluna do meio.
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
                format!("ativa · {}", fmt_dur(age_of(e.last_activity)))
            } else {
                "encerrada".to_string()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(e.kind.label().to_string(), Style::default().add_modifier(Modifier::BOLD)),
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
                ("✓", Color::Green, "veredito (LIMPO)")
            } else {
                ("⚑", Color::Red, "veredito (SUJO)")
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{dot} "), Style::default().fg(color)),
                Span::styled(txt.to_string(), Style::default().add_modifier(Modifier::BOLD)),
            ]))
        }
    }
}

/// Painel direito: conteúdo do card selecionado (chat da sessão, veredito, ou o
/// detalhe da task se não há card).
fn render_card_content(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.board_focus == BoardFocus::Chat || app.chat_fullscreen;
    match app.selected_card() {
        Some(BoardCard::Session(i)) => render_pty(f, app, i, area, focused),
        Some(BoardCard::Verdict) => {
            let id = app.selected_task().map(|t| t.id.clone());
            let lines = verdict_lines(app, id.as_deref());
            let title = match &id {
                Some(i) => format!("Veredito · {i}"),
                None => "Veredito".to_string(),
            };
            let p = Paragraph::new(lines)
                .block(panel_focus(&title, focused))
                .wrap(Wrap { trim: true });
            f.render_widget(p, area);
        }
        None => {
            // sem card: detalhe da task (ou dica).
            let (title, lines) = match app.selected_task() {
                Some(t) => (format!("{} · Enter expande", t.id), task_detail_lines(t)),
                None => (
                    "Detalhes".to_string(),
                    vec![Line::from(Span::styled(
                        "selecione uma task (j/k)",
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

/// Renderiza o PTY da sessão de índice `i` (ou a mensagem de histórico).
fn render_pty(f: &mut Frame, app: &App, i: usize, area: Rect, focused: bool) {
    let hint = if app.pending_prefix {
        "Ctrl+G… (n/p card · f finaliza · x remove · z zoom · g=Ctrl+G)"
    } else if focused {
        "digitando no claude · Ctrl+G = comando jaum"
    } else {
        "l/→ entra no chat"
    };
    let block = if focused {
        panel_tight(&format!("chat — {hint}")).border_style(Style::default().fg(ACCENT))
    } else {
        panel_tight(&format!("chat — {hint}"))
    };
    match app.sessions.get(i) {
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

/// Corpo do veredito (findings + constraints + critérios) para o painel direito.
fn verdict_lines(app: &App, id: Option<&str>) -> Vec<Line<'static>> {
    let report = id.and_then(|i| app.load_review(i));
    let mut lines: Vec<Line> = Vec::new();
    let Some(r) = report else {
        lines.push(Line::from(
            "Sem review ainda. `r` roda o veredito (grava o report); `R` abre o chat de review.",
        ));
        return lines;
    };
    let clean = r.is_clean();
    lines.push(Line::from(vec![
        Span::raw("is_clean: "),
        if clean {
            Span::styled("LIMPO", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("SUJO", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        },
    ]));
    lines.push(Line::from(Span::styled(
        format!(
            "{} bloqueante(s) · {} finding(s) · minor/nit não reprovam",
            r.blocking_count(),
            r.findings.len()
        ),
        Style::default().fg(SUBTLE),
    )));
    if !clean {
        lines.push(Line::from(Span::styled(
            "`H` envia as pendências para a sessão de play corrigir",
            Style::default().fg(SUBTLE),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Findings", Style::default().add_modifier(Modifier::BOLD))));
    if r.findings.is_empty() {
        lines.push(Line::from("  (nenhum)"));
    } else {
        for finding in &r.findings {
            lines.push(Line::from(format!("  {}", finding.render())));
        }
    }
    push_verdict_section(&mut lines, "Constraints (enforce: review)", &r.constraints);
    push_verdict_section(&mut lines, "Critérios de aceite", &r.criteria);
    lines
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


/// Empurra um cabeçalho + os itens de um checklist (constraint/critério) com o
/// veredito colorido, no detalhe da aba Review.
fn push_verdict_section(lines: &mut Vec<Line>, title: &str, items: &[ConstraintResult]) {
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        title.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (nenhum)",
            Style::default().fg(SUBTLE),
        )));
        return;
    }
    for c in items {
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
