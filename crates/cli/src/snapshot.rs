//! Projects the daemon's `App` state into the wire `DomainSnapshot`.
//! Pure read: building a snapshot never mutates the app.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{App, BoardCard, BoardFocus, SessionKind, Tab};
use crate::protocol::{
    BoardView, CardView, ConstraintView, DocsView, DomainSnapshot, EnforceId, FocusId, InputView,
    JobView, OverlapView, PickerView, PrView, ProjectRef, SessionKind as WireSessionKind, StatusId,
    TabId, TaskTypeId, TaskView,
};
use jaum_core::{Enforce, Status, Task, TaskType};

fn status_id(s: Status) -> StatusId {
    match s {
        Status::Backlog => StatusId::Backlog,
        Status::Ready => StatusId::Ready,
        Status::Wip => StatusId::Wip,
        Status::Review => StatusId::Review,
        Status::Merged => StatusId::Merged,
    }
}

fn task_type_id(t: TaskType) -> TaskTypeId {
    match t {
        TaskType::Impl => TaskTypeId::Impl,
        TaskType::Spike => TaskTypeId::Spike,
    }
}

fn enforce_id(e: Enforce) -> EnforceId {
    match e {
        Enforce::Hook => EnforceId::Hook,
        Enforce::Review => EnforceId::Review,
    }
}

pub(crate) fn wire_session_kind(k: SessionKind) -> WireSessionKind {
    match k {
        SessionKind::Play => WireSessionKind::Play,
        SessionKind::Setup => WireSessionKind::Setup,
    }
}

/// Epoch milliseconds truncated to the second: sub-second churn (PTY output
/// bumps `last_activity` every drain) would make consecutive snapshots
/// compare as different and defeat the coalescing dedupe.
fn epoch_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() * 1000
}

fn task_view(app: &App, t: &Task) -> TaskView {
    TaskView {
        id: t.id.clone(),
        task_type: task_type_id(t.task_type),
        status: status_id(t.status),
        rfcs: t.rfcs.clone(),
        adrs: t.adrs.clone(),
        prs: t
            .prs
            .iter()
            .map(|p| PrView {
                repo: p.repo.clone(),
                pr: p.pr,
                branch: p.branch.clone(),
            })
            .collect(),
        deferred: t.deferred.clone(),
        constraints: t
            .constraints
            .iter()
            .map(|c| ConstraintView {
                text: c.text.clone(),
                enforce: enforce_id(c.enforce),
            })
            .collect(),
        body: t.body.clone(),
        // a background session of another project may carry the same task id
        // (unique only within one project's `.backlog/`); it must not light up
        // this task's badge.
        live_session: app.sessions.iter().any(|e| {
            e.is_live()
                && e.project == app.project_name()
                && e.task.as_deref() == Some(t.id.as_str())
        }),
    }
}

fn card_view(app: &App, card: BoardCard) -> CardView {
    match card {
        BoardCard::Session(i) => {
            let e = &app.sessions[i];
            CardView::Session {
                kind: wire_session_kind(e.kind),
                live: e.is_live(),
                last_activity_ms: epoch_ms(e.last_activity),
            }
        }
    }
}

/// Builds the full snapshot of the app's domain state.
pub fn build_snapshot(app: &App) -> DomainSnapshot {
    let tasks: Vec<TaskView> = app.tasks.iter().map(|t| task_view(app, t)).collect();

    let cards: Vec<CardView> = app
        .task_cards()
        .into_iter()
        .map(|c| card_view(app, c))
        .collect();

    let board = BoardView {
        tasks,
        selected: app.selected,
        project_selected: app.project_selected,
        focus: match app.board_focus {
            BoardFocus::Tasks => FocusId::Tasks,
            BoardFocus::Cards => FocusId::Cards,
            BoardFocus::Chat => FocusId::Chat,
        },
        cards,
        card_selected: app.card_selected,
        chat_fullscreen: app.chat_fullscreen,
        setup_needed: app.setup_needed(),
        setup_live: app
            .sessions
            .iter()
            .any(|e| e.is_live() && e.kind == SessionKind::Setup),
        detail_open: app.detail_open,
        detail_scroll: app.detail_scroll,
        overlaps: app
            .overlaps
            .iter()
            .map(|(a, b, repo)| OverlapView {
                a: a.clone(),
                b: b.clone(),
                repo: repo.clone(),
            })
            .collect(),
    };

    // the preview is only read (and shipped) while something displays it,
    // mirroring the old render path; snapshots are built every tick, so an
    // unconditional read would hit the disk at 60/s for nothing.
    let preview_visible = app.tab == Tab::Docs || app.doc_open;
    let docs = DocsView {
        dir: app.docs_dir.display().to_string(),
        list: app.docs.clone(),
        selected: app.docs_selected,
        preview: if preview_visible {
            app.docs
                .get(app.docs_selected)
                .map(|rel| std::fs::read_to_string(app.docs_dir.join(rel)).unwrap_or_default())
                .unwrap_or_default()
        } else {
            String::new()
        },
        doc_open: app.doc_open,
        scroll: app.doc_scroll,
    };

    DomainSnapshot {
        project: app.project_name().to_string(),
        projects: app
            .config
            .projects
            .iter()
            .map(|p| ProjectRef {
                name: p.name.clone(),
                backlog: p.backlog.display().to_string(),
            })
            .collect(),
        tab: match app.tab {
            Tab::Board => TabId::Board,
            Tab::Docs => TabId::Docs,
        },
        board,
        docs,
        picker: app.project_picker.then_some(PickerView {
            selected: app.picker_selected,
        }),
        input: app.input.as_ref().map(|(kind, buffer)| InputView {
            kind: *kind,
            buffer: buffer.clone(),
        }),
        job: app.job.as_ref().map(|j| JobView {
            title: j.title.clone(),
            logs: j.logs.clone(),
            finished: j.finished,
            follow: j.follow,
            scroll: j.scroll,
        }),
        job_overlay: app.job_overlay,
        toast: app.active_toast().map(str::to_string),
    }
}

// Unit tests live in-crate (not under tests/) so llvm-cov attributes the
// exercised lines to this file.
#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod snapshot_tests;
