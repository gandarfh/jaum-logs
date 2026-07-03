//! Tests for the key → intent mapping. Contexts are built by hand (the
//! mapping is pure), plus a couple of `from_snapshot`/`from_app` checks.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{KeyCtx, map_key, with_local_prefill};
use crate::protocol::{CardView, FocusId, InputKind, Intent, SessionKind, TabId};

fn base_ctx() -> KeyCtx {
    KeyCtx {
        job_overlay: false,
        picker_open: false,
        detail_open: false,
        doc_open: false,
        input_active: false,
        tab: TabId::Board,
        focus: FocusId::Tasks,
        chat_live: false,
        has_task: true,
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[test]
fn job_overlay_captures_navigation() {
    let ctx = KeyCtx {
        job_overlay: true,
        ..base_ctx()
    };
    assert_eq!(map_key(&ctx, key(KeyCode::Esc)), Some(Intent::DismissJob));
    assert_eq!(map_key(&ctx, ch('q')), Some(Intent::DismissJob));
    assert_eq!(map_key(&ctx, ch('j')), Some(Intent::JobScrollDown));
    assert_eq!(map_key(&ctx, ch('k')), Some(Intent::JobScrollUp));
    assert_eq!(map_key(&ctx, ch('g')), Some(Intent::JobScrollTop));
    assert_eq!(map_key(&ctx, ch('G')), Some(Intent::JobFollow));
    assert_eq!(map_key(&ctx, ch('p')), None, "actions are locked out");
}

#[test]
fn picker_overlay_captures_navigation() {
    let ctx = KeyCtx {
        picker_open: true,
        ..base_ctx()
    };
    assert_eq!(map_key(&ctx, ch('j')), Some(Intent::PickerNext));
    assert_eq!(map_key(&ctx, ch('k')), Some(Intent::PickerPrev));
    assert_eq!(
        map_key(&ctx, key(KeyCode::Enter)),
        Some(Intent::PickerConfirm)
    );
    assert_eq!(map_key(&ctx, key(KeyCode::Esc)), Some(Intent::ClosePicker));
    assert_eq!(map_key(&ctx, ch('x')), None);
}

#[test]
fn detail_overlay_scrolls_and_closes() {
    let ctx = KeyCtx {
        detail_open: true,
        ..base_ctx()
    };
    assert_eq!(map_key(&ctx, ch('j')), Some(Intent::DetailScrollDown));
    assert_eq!(map_key(&ctx, ch('k')), Some(Intent::DetailScrollUp));
    assert_eq!(
        map_key(&ctx, key(KeyCode::Enter)),
        Some(Intent::CloseDetail)
    );
    assert_eq!(map_key(&ctx, ch('q')), Some(Intent::CloseDetail));
    assert_eq!(map_key(&ctx, ch('z')), None);
}

#[test]
fn doc_overlay_scrolls_and_closes() {
    let ctx = KeyCtx {
        doc_open: true,
        ..base_ctx()
    };
    assert_eq!(map_key(&ctx, ch('j')), Some(Intent::DocScrollDown));
    assert_eq!(map_key(&ctx, ch('k')), Some(Intent::DocScrollUp));
    assert_eq!(map_key(&ctx, key(KeyCode::Esc)), Some(Intent::CloseDoc));
    assert_eq!(map_key(&ctx, key(KeyCode::Enter)), None);
}

#[test]
fn input_capture_takes_every_char() {
    let ctx = KeyCtx {
        input_active: true,
        ..base_ctx()
    };
    assert_eq!(map_key(&ctx, ch('a')), Some(Intent::InputChar { ch: 'a' }));
    // even action letters are text while capturing
    assert_eq!(map_key(&ctx, ch('q')), Some(Intent::InputChar { ch: 'q' }));
    assert_eq!(
        map_key(&ctx, key(KeyCode::Backspace)),
        Some(Intent::InputBackspace)
    );
    assert_eq!(
        map_key(&ctx, key(KeyCode::Enter)),
        Some(Intent::InputSubmit)
    );
    assert_eq!(map_key(&ctx, key(KeyCode::Esc)), Some(Intent::InputCancel));
    assert_eq!(map_key(&ctx, key(KeyCode::Home)), None);
}

#[test]
fn chat_focus_swallows_keys_except_esc_and_quit() {
    let ctx = KeyCtx {
        focus: FocusId::Chat,
        chat_live: true,
        ..base_ctx()
    };
    assert_eq!(map_key(&ctx, key(KeyCode::Esc)), Some(Intent::FocusLeft));
    // with no PTY behind the placeholder, quitting must stay possible
    assert_eq!(map_key(&ctx, ch('q')), Some(Intent::Quit));
    assert_eq!(map_key(&ctx, ctrl('c')), Some(Intent::Quit));
    assert_eq!(map_key(&ctx, ch('j')), None, "reserved for the session");
    assert_eq!(map_key(&ctx, ch('p')), None);

    // a dead session releases the keys back to navigation
    let ctx = KeyCtx {
        focus: FocusId::Chat,
        chat_live: false,
        ..base_ctx()
    };
    assert_eq!(map_key(&ctx, ch('q')), Some(Intent::Quit));
}

#[test]
fn navigation_depends_on_tab_and_focus() {
    let tasks = base_ctx();
    assert_eq!(map_key(&tasks, ch('j')), Some(Intent::SelectNext));
    assert_eq!(map_key(&tasks, ch('k')), Some(Intent::SelectPrev));
    assert_eq!(map_key(&tasks, ch('g')), Some(Intent::SelectFirst));
    assert_eq!(map_key(&tasks, ch('G')), Some(Intent::SelectLast));
    assert_eq!(map_key(&tasks, ch('l')), Some(Intent::FocusRight));
    assert_eq!(map_key(&tasks, ch('h')), Some(Intent::FocusLeft));
    assert_eq!(
        map_key(&tasks, key(KeyCode::Enter)),
        Some(Intent::OpenDetail)
    );

    let cards = KeyCtx {
        focus: FocusId::Cards,
        ..base_ctx()
    };
    assert_eq!(map_key(&cards, ch('j')), Some(Intent::CardNext));
    assert_eq!(map_key(&cards, ch('k')), Some(Intent::CardPrev));
    // Enter on a live session card enters the chat
    let live_cards = KeyCtx {
        chat_live: true,
        ..cards
    };
    assert_eq!(
        map_key(&live_cards, key(KeyCode::Enter)),
        Some(Intent::FocusRight)
    );

    let docs = KeyCtx {
        tab: TabId::Docs,
        ..base_ctx()
    };
    assert_eq!(map_key(&docs, ch('j')), Some(Intent::DocsNext));
    assert_eq!(map_key(&docs, ch('k')), Some(Intent::DocsPrev));
    assert_eq!(map_key(&docs, ch('J')), Some(Intent::DocScrollDown));
    assert_eq!(map_key(&docs, ch('K')), Some(Intent::DocScrollUp));
    assert_eq!(map_key(&docs, ch('l')), Some(Intent::NextTab));
    assert_eq!(map_key(&docs, ch('h')), Some(Intent::NextTab));
    assert_eq!(map_key(&docs, key(KeyCode::Enter)), Some(Intent::OpenDoc));
    // Shift+J outside Docs falls through to nothing
    assert_eq!(map_key(&tasks, ch('J')), None);
}

#[test]
fn actions_map_to_intents() {
    let ctx = base_ctx();
    assert_eq!(map_key(&ctx, ch('q')), Some(Intent::Quit));
    assert_eq!(map_key(&ctx, ctrl('c')), Some(Intent::Quit));
    assert_eq!(map_key(&ctx, key(KeyCode::Tab)), Some(Intent::NextTab));
    assert_eq!(map_key(&ctx, ch('2')), Some(Intent::SetTab { index: 1 }));
    assert_eq!(map_key(&ctx, ch('z')), Some(Intent::ToggleZoom));
    assert_eq!(map_key(&ctx, ch('p')), Some(Intent::Play));
    // `r` no longer triggers a manual verdict: CI runs it automatically now
    assert_eq!(map_key(&ctx, ch('r')), None);
    assert_eq!(map_key(&ctx, ch('R')), Some(Intent::ReviewChat));
    assert_eq!(map_key(&ctx, ch('H')), Some(Intent::Handoff));
    assert_eq!(map_key(&ctx, ch('f')), Some(Intent::Finish));
    assert_eq!(map_key(&ctx, ch('i')), Some(Intent::Ingest));
    assert_eq!(map_key(&ctx, ch('a')), Some(Intent::AnalyzeParallel));
    assert_eq!(map_key(&ctx, ch('S')), Some(Intent::StartSetup));
    assert_eq!(map_key(&ctx, ch('P')), Some(Intent::OpenPicker));
    assert_eq!(map_key(&ctx, ch('e')), Some(Intent::EditConventions));
    assert_eq!(
        map_key(&ctx, ch('c')),
        Some(Intent::StartInput {
            kind: InputKind::Convention,
            prefill: String::new()
        })
    );
    assert_eq!(
        map_key(&ctx, ch('n')),
        Some(Intent::StartInput {
            kind: InputKind::NewTask,
            prefill: String::new()
        })
    );
    assert_eq!(
        map_key(&ctx, ch('N')),
        Some(Intent::StartInput {
            kind: InputKind::NewTaskClaude,
            prefill: String::new()
        })
    );
    assert_eq!(
        map_key(&ctx, ch('I')),
        Some(Intent::StartInput {
            kind: InputKind::InitPath,
            prefill: String::new()
        })
    );
    assert_eq!(
        map_key(&ctx, ch('d')),
        Some(Intent::StartInput {
            kind: InputKind::Defer,
            prefill: String::new()
        })
    );
    assert_eq!(map_key(&ctx, key(KeyCode::F(5))), None);

    // `d` needs a selected task
    let no_task = KeyCtx {
        has_task: false,
        ..base_ctx()
    };
    assert_eq!(map_key(&no_task, ch('d')), None);
}

#[test]
fn init_prefill_uses_local_cwd() {
    let filled = with_local_prefill(Intent::StartInput {
        kind: InputKind::InitPath,
        prefill: String::new(),
    });
    match filled {
        Intent::StartInput { kind, prefill } => {
            assert_eq!(kind, InputKind::InitPath);
            assert!(!prefill.is_empty(), "cwd should be prefilled");
        }
        other => panic!("wrong intent: {other:?}"),
    }
    // non-init intents pass through untouched
    assert_eq!(with_local_prefill(Intent::Quit), Intent::Quit);
    let conv = Intent::StartInput {
        kind: InputKind::Convention,
        prefill: String::new(),
    };
    assert_eq!(with_local_prefill(conv.clone()), conv);
}

#[test]
fn ctx_from_snapshot_reads_ui_mode() {
    let mut snap = crate::protocol::tests::sample_snapshot();
    snap.picker = None;
    snap.input = None;
    snap.job_overlay = false;
    let ctx = KeyCtx::from_snapshot(&snap);
    assert_eq!(ctx.tab, TabId::Board);
    assert_eq!(ctx.focus, FocusId::Cards);
    assert!(ctx.chat_live, "selected card is a live session");
    assert!(ctx.has_task);

    // selecting the verdict card kills chat_live
    snap.board.card_selected = 1;
    let ctx = KeyCtx::from_snapshot(&snap);
    assert!(!ctx.chat_live);

    // a cursor past the last card clamps to it (mirrors selected_card)
    snap.board.card_selected = 9;
    let ctx = KeyCtx::from_snapshot(&snap);
    assert!(!ctx.chat_live);

    // the project row has no task
    snap.board.project_selected = true;
    snap.board.cards = vec![CardView::Session {
        kind: SessionKind::Setup,
        live: true,
        last_activity_ms: 0,
    }];
    snap.board.card_selected = 0;
    let ctx = KeyCtx::from_snapshot(&snap);
    assert!(!ctx.has_task);
    assert!(ctx.chat_live);

    // overlays flow through
    snap.job_overlay = true;
    snap.docs.doc_open = true;
    snap.board.detail_open = true;
    let ctx = KeyCtx::from_snapshot(&snap);
    assert!(ctx.job_overlay && ctx.doc_open && ctx.detail_open);
}
