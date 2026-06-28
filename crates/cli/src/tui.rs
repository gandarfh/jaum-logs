//! Render (ratatui) e event loop (crossterm) da TUI.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, Tab, status_label};
use jaum_flows::review::ConstraintVerdict;

pub fn run(mut app: App) -> Result<()> {
    let mut terminal = ratatui::init();
    let res = run_loop(&mut terminal, &mut app);
    ratatui::restore();
    res
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.drain_pty();
        sync_pty_size(terminal, app);
        terminal.draw(|f| render(f, app))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            handle_key(app, k);
        }
        if app.should_quit {
            app.stop_session();
            break;
        }
    }
    Ok(())
}

/// Mantém o PTY e o parser vt100 no tamanho EXATO do pane Session (corpo menos
/// as bordas). Sem isso, o `claude` quebra linha numa largura diferente da
/// renderizada e o texto sai corrompido.
fn sync_pty_size(terminal: &DefaultTerminal, app: &mut App) {
    let Ok(size) = terminal.size() else { return };
    // layout: tabs(3) + corpo + statusline(1); o pane tem borda (-2 em cada eixo)
    let cols = size.width.saturating_sub(2);
    let rows = size.height.saturating_sub(3 + 1 + 2);
    if cols == 0 || rows == 0 {
        return;
    }
    if let (Some(session), Some(parser)) = (&app.session, &mut app.parser)
        && parser.screen().size() != (rows, cols)
    {
        let _ = session.resize(rows, cols);
        parser.screen_mut().set_size(rows, cols);
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
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

    // 1) captura de texto (defer)
    if let Some(buf) = app.input.as_mut() {
        match key.code {
            KeyCode::Esc => app.input = None,
            KeyCode::Enter => {
                let text = app.input.take().unwrap_or_default();
                app.defer(&text);
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
        return;
    }

    // 2) aba Session com sessão viva: TODAS as teclas vão para o `claude`.
    //    Para comandos do jaum, use o prefixo Ctrl+B (estilo tmux):
    //      Ctrl+B 1-4 troca aba · Ctrl+B q sai · Ctrl+B x encerra sessão · Ctrl+B b envia Ctrl+B
    if app.tab == Tab::Session && app.session.is_some() {
        if app.pending_prefix {
            app.pending_prefix = false;
            match key.code {
                KeyCode::Char(c @ '1'..='4') => {
                    app.tab = Tab::from_index(c as usize - '1' as usize)
                }
                KeyCode::Char('q') => app.should_quit = true,
                KeyCode::Char('x') => app.stop_session(),
                KeyCode::Char('b') => {
                    if let Some(s) = app.session.as_mut() {
                        let _ = s.write_input(&[0x02]); // Ctrl+B literal
                    }
                }
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
            app.pending_prefix = true;
            return;
        }
        if let Some(session) = app.session.as_mut() {
            let bytes = key_to_bytes(key);
            if !bytes.is_empty() {
                let _ = session.write_input(&bytes);
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
        // vim: j/k cima-baixo, g/G topo-fim, h/l troca de aba
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),
        KeyCode::Char('l') | KeyCode::Right => app.tab = app.tab.next(),
        KeyCode::Char('h') | KeyCode::Left => app.tab = app.tab.prev(),
        KeyCode::Enter | KeyCode::Char('o') => app.open_detail(),
        KeyCode::Char('p') => app.play_selected(),
        KeyCode::Char('r') => app.review_selected(),
        KeyCode::Char('f') => app.finish_selected(),
        KeyCode::Char('i') => app.ingest(),
        KeyCode::Char('P') => app.open_picker(),
        KeyCode::Char('d') if app.selected_task().is_some() => {
            app.input = Some(String::new());
            app.status_msg = "defer: digite o escopo extra, Enter confirma, Esc cancela".into();
        }
        _ => {}
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

fn render(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tabs
            Constraint::Min(0),    // corpo
            Constraint::Length(1), // statusline
        ])
        .split(f.area());

    render_tabs(f, app, chunks[0]);
    match app.tab {
        Tab::Board => render_board(f, app, chunks[1]),
        Tab::Session => render_session(f, app, chunks[1]),
        Tab::Review => render_review(f, app, chunks[1]),
        Tab::Docs => render_docs(f, app, chunks[1]),
    }
    render_statusline(f, app, chunks[2]);

    if app.detail_open {
        render_detail(f, app);
    }
    if app.project_picker {
        render_picker(f, app);
    }
}

/// Overlay com o conteúdo completo da task selecionada (Enter/`o`).
fn render_detail(f: &mut Frame, app: &App) {
    use ratatui::widgets::Clear;

    let Some(t) = app.selected_task() else { return };
    let area = centered_rect(80, 80, f.area());
    f.render_widget(Clear, area);

    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(format!("{}  ", t.id), bold),
        Span::styled(format!("{:?}", t.task_type), Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(status_label(t.status), Style::default().fg(Color::Yellow)),
    ]));

    if !t.rfcs.is_empty() {
        lines.push(Line::from(format!("RFCs: {}", t.rfcs.join(", "))));
    }
    if !t.adrs.is_empty() {
        lines.push(Line::from(format!("ADRs: {}", t.adrs.join(", "))));
    }
    for pr in &t.prs {
        let n = if pr.pr == 0 {
            "PR não criado".to_string()
        } else {
            format!("PR #{}", pr.pr)
        };
        lines.push(Line::from(format!("repo {} @ {} ({n})", pr.repo, pr.branch)));
    }
    if !t.constraints.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Constraints", bold)));
        for c in &t.constraints {
            let (tag, color) = match c.enforce {
                jaum_core::Enforce::Hook => ("hook", Color::Red),
                jaum_core::Enforce::Review => ("review", Color::Magenta),
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

    // corpo (markdown: Objetivo, Criterio de aceite)
    lines.push(Line::from(""));
    for raw in t.body.lines() {
        lines.push(Line::from(raw.to_string()));
    }

    let title = format!(" {} · j/k rola · Esc fecha ", t.id);
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    f.render_widget(p, area);
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = Tab::all()
        .iter()
        .enumerate()
        .map(|(i, t)| Line::from(format!(" {}:{} ", i + 1, t.title())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" jaum-logs · {} ", app.project_name())),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, area);
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Projects · Enter troca · Esc fecha "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("➤ ");
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
    let mut items: Vec<ListItem> = Vec::new();
    let mut row_to_task: Vec<Option<usize>> = Vec::new();
    let mut last_status: Option<jaum_core::Status> = None;

    for (i, t) in app.tasks.iter().enumerate() {
        if last_status != Some(t.status) {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("── {} ──", status_label(t.status).to_uppercase()),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ))));
            row_to_task.push(None);
            last_status = Some(t.status);
        }

        let mut spans = vec![Span::raw(format!("  {:<10} ", t.id))];
        if t.status == jaum_core::Status::Wip {
            spans.push(Span::styled("▶ play ", Style::default().fg(Color::Green)));
        }
        if t.status == jaum_core::Status::Review
            && let Some(n) = app.review_badge(&t.id)
        {
            let color = if n == 0 { Color::Green } else { Color::Yellow };
            spans.push(Span::styled(format!("⚑ {n} "), Style::default().fg(color)));
        }
        if let Some(pr) = t.prs.first() {
            spans.push(Span::styled(
                pr.branch.clone(),
                Style::default().fg(Color::Blue),
            ));
        }
        items.push(ListItem::new(Line::from(spans)));
        row_to_task.push(Some(i));
    }

    if items.is_empty() {
        items.push(ListItem::new("(backlog vazio)"));
    }

    let selected_row = row_to_task.iter().position(|r| *r == Some(app.selected));
    let mut state = ListState::default();
    state.select(selected_row);

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Board "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("➤ ");
    f.render_stateful_widget(list, area, &mut state);
}

fn render_session(f: &mut Frame, app: &App, area: Rect) {
    let hint = if app.pending_prefix {
        "Ctrl+B… (1-4 aba · q sai · x encerra · b=Ctrl+B)"
    } else if app.session.is_some() {
        "digitando no claude · Ctrl+B = comando jaum"
    } else {
        "p play · r review"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Session — {hint} "));
    match &app.parser {
        Some(parser) => {
            let term = PseudoTerminal::new(parser.screen()).block(block);
            f.render_widget(term, area);
        }
        None => {
            let p = Paragraph::new(
                "Sem sessão. Selecione uma task no Board e tecle `p` (play) ou `r` (review).",
            )
            .block(block)
            .wrap(Wrap { trim: true });
            f.render_widget(p, area);
        }
    }
}

fn render_review(f: &mut Frame, app: &App, area: Rect) {
    let id = app.selected_task().map(|t| t.id.clone());
    let report = id.as_deref().and_then(|i| app.load_review(i));

    let mut lines: Vec<Line> = Vec::new();
    match report {
        None => lines.push(Line::from("Sem review. Selecione a task e tecle `r`.")),
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
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Review "))
        .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn render_docs(f: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&app.docs_dir) {
        let mut names: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| (n.starts_with("RFC-") || n.starts_with("ADR-")) && n.ends_with(".md"))
            .collect();
        names.sort();
        for n in names {
            items.push(ListItem::new(n));
        }
    }
    if items.is_empty() {
        items.push(ListItem::new(format!(
            "(nenhum RFC/ADR em {})",
            app.docs_dir.display()
        )));
    }
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Docs (RFC/ADR) "),
    );
    f.render_widget(list, area);
}

fn render_statusline(f: &mut Frame, app: &App, area: Rect) {
    let text = match &app.input {
        Some(buf) => format!("defer> {buf}"),
        None => format!("{}  —  {}", app.statusline(), app.status_msg),
    };
    let p =
        Paragraph::new(text).style(Style::default().fg(Color::White).bg(Color::Rgb(30, 30, 40)));
    f.render_widget(p, area);
}
