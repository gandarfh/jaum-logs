//! Key → `Intent` mapping shared by the local TUI and the socket client.
//! The mapping is a pure function of the UI mode (`KeyCtx`), so both sides
//! stay in lockstep: the client derives the context from the last
//! `DomainSnapshot`, the local TUI derives it from the `App`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, BoardFocus, Tab};
use crate::protocol::{CardView, DomainSnapshot, FocusId, InputKind, Intent, TabId};

/// UI mode relevant to key dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCtx {
    pub job_overlay: bool,
    pub picker_open: bool,
    pub detail_open: bool,
    pub doc_open: bool,
    pub input_active: bool,
    pub tab: TabId,
    pub focus: FocusId,
    /// The selected card is a live session (chat reachable).
    pub chat_live: bool,
    /// A task row is selected (not the `· project` row).
    pub has_task: bool,
}

impl KeyCtx {
    pub fn from_app(app: &App) -> Self {
        Self {
            job_overlay: app.job_overlay,
            picker_open: app.project_picker,
            detail_open: app.detail_open,
            doc_open: app.doc_open,
            input_active: app.input.is_some(),
            tab: match app.tab {
                Tab::Board => TabId::Board,
                Tab::Docs => TabId::Docs,
            },
            focus: match app.board_focus {
                BoardFocus::Tasks => FocusId::Tasks,
                BoardFocus::Cards => FocusId::Cards,
                BoardFocus::Chat => FocusId::Chat,
            },
            chat_live: app.selected_card_is_live(),
            has_task: app.selected_task().is_some(),
        }
    }

    pub fn from_snapshot(s: &DomainSnapshot) -> Self {
        let cards = &s.board.cards;
        let idx = s.board.card_selected.min(cards.len().saturating_sub(1));
        let chat_live = matches!(cards.get(idx), Some(CardView::Session { live: true, .. }));
        Self {
            job_overlay: s.job_overlay,
            picker_open: s.picker.is_some(),
            detail_open: s.board.detail_open,
            doc_open: s.docs.doc_open,
            input_active: s.input.is_some(),
            tab: s.tab,
            focus: s.board.focus,
            chat_live,
            has_task: !s.board.project_selected && s.board.selected < s.board.tasks.len(),
        }
    }
}

/// Fills the `init` input with the local working directory. The prefill comes
/// from the machine where the user typed, not from the daemon's cwd.
pub fn with_local_prefill(mut intent: Intent) -> Intent {
    if let Intent::StartInput {
        kind: InputKind::InitPath,
        prefill,
    } = &mut intent
        && prefill.is_empty()
    {
        *prefill = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    intent
}

/// Maps a key press to a domain intent given the UI mode. Returns `None` for
/// keys that mean nothing in the current mode (the local chat pane handles its
/// own keys BEFORE this, writing straight to the PTY).
pub fn map_key(ctx: &KeyCtx, key: KeyEvent) -> Option<Intent> {
    // 0) job log overlay (ingest/capture/init)
    if ctx.job_overlay {
        return match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Some(Intent::DismissJob),
            KeyCode::Char('k') | KeyCode::Up => Some(Intent::JobScrollUp),
            KeyCode::Char('j') | KeyCode::Down => Some(Intent::JobScrollDown),
            KeyCode::Char('g') | KeyCode::Home => Some(Intent::JobScrollTop),
            KeyCode::Char('G') | KeyCode::End => Some(Intent::JobFollow),
            _ => None,
        };
    }

    // 0) project picker (overlay)
    if ctx.picker_open {
        return match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Some(Intent::ClosePicker),
            KeyCode::Char('j') | KeyCode::Down => Some(Intent::PickerNext),
            KeyCode::Char('k') | KeyCode::Up => Some(Intent::PickerPrev),
            KeyCode::Enter => Some(Intent::PickerConfirm),
            _ => None,
        };
    }

    // 0.5) task detail overlay
    if ctx.detail_open {
        return match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => Some(Intent::CloseDetail),
            KeyCode::Char('j') | KeyCode::Down => Some(Intent::DetailScrollDown),
            KeyCode::Char('k') | KeyCode::Up => Some(Intent::DetailScrollUp),
            _ => None,
        };
    }

    // 0.6) doc view overlay (markdown)
    if ctx.doc_open {
        return match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Some(Intent::CloseDoc),
            KeyCode::Char('j') | KeyCode::Down => Some(Intent::DocScrollDown),
            KeyCode::Char('k') | KeyCode::Up => Some(Intent::DocScrollUp),
            _ => None,
        };
    }

    // 1) text capture (defer / convention / new task / init)
    if ctx.input_active {
        return match key.code {
            KeyCode::Esc => Some(Intent::InputCancel),
            KeyCode::Enter => Some(Intent::InputSubmit),
            KeyCode::Backspace => Some(Intent::InputBackspace),
            KeyCode::Char(ch) => Some(Intent::InputChar { ch }),
            _ => None,
        };
    }

    // 2) chat focus without a local PTY (socket client): only Esc leaves the
    //    chat; every other key is swallowed until the session panel lands.
    if ctx.tab == TabId::Board && ctx.focus == FocusId::Chat && ctx.chat_live {
        return match key.code {
            KeyCode::Esc if key.modifiers.is_empty() => Some(Intent::FocusLeft),
            _ => None,
        };
    }

    // Ctrl+C quits (in raw mode it doesn't become SIGINT; handle it here)
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(Intent::Quit);
    }

    // 3) navigation (list focus: Tasks/Cards on the Board, or Docs)
    match key.code {
        KeyCode::Char('q') => Some(Intent::Quit),
        KeyCode::Tab => Some(Intent::NextTab),
        KeyCode::Char(c @ '1'..='2') => Some(Intent::SetTab {
            index: c as usize - '1' as usize,
        }),
        // j/k navigate WITHIN the focused panel; h/l move focus (Tasks↔Cards↔Chat).
        KeyCode::Char('j') | KeyCode::Down => Some(match (ctx.tab, ctx.focus) {
            (TabId::Docs, _) => Intent::DocsNext,
            (TabId::Board, FocusId::Cards) => Intent::CardNext,
            (TabId::Board, _) => Intent::SelectNext,
        }),
        KeyCode::Char('k') | KeyCode::Up => Some(match (ctx.tab, ctx.focus) {
            (TabId::Docs, _) => Intent::DocsPrev,
            (TabId::Board, FocusId::Cards) => Intent::CardPrev,
            (TabId::Board, _) => Intent::SelectPrev,
        }),
        // Shift+J/K scrolls the doc preview without opening the overlay.
        KeyCode::Char('J') if ctx.tab == TabId::Docs => Some(Intent::DocScrollDown),
        KeyCode::Char('K') if ctx.tab == TabId::Docs => Some(Intent::DocScrollUp),
        KeyCode::Char('g') | KeyCode::Home => Some(Intent::SelectFirst),
        KeyCode::Char('G') | KeyCode::End => Some(Intent::SelectLast),
        KeyCode::Char('l') | KeyCode::Right => Some(if ctx.tab == TabId::Board {
            Intent::FocusRight
        } else {
            Intent::NextTab
        }),
        KeyCode::Char('h') | KeyCode::Left => Some(if ctx.tab == TabId::Board {
            Intent::FocusLeft
        } else {
            Intent::NextTab
        }),
        KeyCode::Enter | KeyCode::Char('o') => Some(if ctx.tab == TabId::Docs {
            Intent::OpenDoc
        } else if ctx.focus == FocusId::Cards && ctx.chat_live {
            Intent::FocusRight
        } else {
            Intent::OpenDetail
        }),
        KeyCode::Char('z') => Some(Intent::ToggleZoom),
        KeyCode::Char('p') => Some(Intent::Play),
        KeyCode::Char('R') => Some(Intent::ReviewChat),
        KeyCode::Char('H') => Some(Intent::Handoff),
        KeyCode::Char('f') => Some(Intent::Finish),
        KeyCode::Char('i') => Some(Intent::Ingest),
        KeyCode::Char('I') => Some(Intent::StartInput {
            kind: InputKind::InitPath,
            prefill: String::new(),
        }),
        KeyCode::Char('a') => Some(Intent::AnalyzeParallel),
        KeyCode::Char('S') => Some(Intent::StartSetup),
        KeyCode::Char('P') => Some(Intent::OpenPicker),
        KeyCode::Char('e') => Some(Intent::EditConventions),
        // quick capture
        KeyCode::Char('c') => Some(Intent::StartInput {
            kind: InputKind::Convention,
            prefill: String::new(),
        }),
        KeyCode::Char('n') => Some(Intent::StartInput {
            kind: InputKind::NewTask,
            prefill: String::new(),
        }),
        KeyCode::Char('N') => Some(Intent::StartInput {
            kind: InputKind::NewTaskClaude,
            prefill: String::new(),
        }),
        KeyCode::Char('d') if ctx.has_task => Some(Intent::StartInput {
            kind: InputKind::Defer,
            prefill: String::new(),
        }),
        _ => None,
    }
}

// Unit tests live in-crate (not under tests/) so llvm-cov attributes the
// exercised lines to this file.
#[cfg(test)]
#[path = "keymap_tests.rs"]
mod keymap_tests;
