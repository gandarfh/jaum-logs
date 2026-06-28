use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_core::Status;

// Os módulos app/config são privados do binário; reexercitamos a lógica pura
// recompilando-os como parte do crate de teste. (app.rs referencia
// `crate::config`, então config precisa estar declarado aqui também.)
#[path = "../src/app.rs"]
#[allow(dead_code)]
mod app;
#[path = "../src/config.rs"]
#[allow(dead_code)]
mod config;

use app::{App, STATUS_ORDER, Tab, sort_for_board, status_label};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-app-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn task_md(id: &str, status: &str) -> String {
    format!(
        "---\nid: {id}\ntype: impl\nstatus: {status}\nprs:\n  - repo: org/x\n    pr: 0\n    branch: feat/{id}\n---\n\n## Objetivo\nx\n"
    )
}

fn app_with(dir: &TmpDir, tasks: &[(&str, &str)]) -> App {
    let backlog = dir.0.join(".backlog");
    fs::create_dir_all(&backlog).unwrap();
    for (id, status) in tasks {
        fs::write(backlog.join(format!("{id}.md")), task_md(id, status)).unwrap();
    }
    let project = config::Project {
        name: "test".into(),
        backlog,
        docs: dir.0.join("docs"),
        work_dir: dir.0.join(".jaum"),
        repos: Vec::new(),
    };
    let cfg = config::Config {
        projects: vec![project],
    };
    App::new(cfg, 0).unwrap()
}

#[test]
fn tab_navega_em_ciclo() {
    assert_eq!(Tab::Board.next(), Tab::Session);
    assert_eq!(Tab::Docs.next(), Tab::Board);
    assert_eq!(Tab::from_index(2), Tab::Review);
    assert_eq!(Tab::Review.index(), 2);
}

#[test]
fn vim_nav_tab_prev_e_select_first_last() {
    assert_eq!(Tab::Board.prev(), Tab::Docs);
    assert_eq!(Tab::Session.prev(), Tab::Board);

    let dir = TmpDir::new("vim");
    let mut app = app_with(
        &dir,
        &[
            ("TASK-001", "wip"),
            ("TASK-002", "wip"),
            ("TASK-003", "wip"),
        ],
    );
    app.select_last();
    assert_eq!(app.selected, 2);
    app.select_first();
    assert_eq!(app.selected, 0);
}

#[test]
fn sort_for_board_agrupa_por_status_canonico() {
    let dir = TmpDir::new("sort");
    let app = app_with(
        &dir,
        &[
            ("TASK-001", "backlog"),
            ("TASK-002", "wip"),
            ("TASK-003", "review"),
        ],
    );
    // wip vem antes de review, que vem antes de backlog
    let ids: Vec<&str> = app.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["TASK-002", "TASK-003", "TASK-001"]);
    assert_eq!(STATUS_ORDER[0], Status::Wip);
    assert_eq!(status_label(Status::Wip), "wip");
}

#[test]
fn sort_for_board_eh_estavel_por_id() {
    let dir = TmpDir::new("sortid");
    let app = app_with(&dir, &[("TASK-003", "wip"), ("TASK-001", "wip")]);
    let ids: Vec<&str> = app.tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["TASK-001", "TASK-003"]);
    let _ = sort_for_board(Vec::new()); // não panica vazio
}

#[test]
fn detalhe_abre_so_com_task_selecionada_e_fecha() {
    let dir = TmpDir::new("detail");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    assert!(!app.detail_open);
    app.open_detail();
    assert!(app.detail_open);
    app.detail_scroll_down();
    assert_eq!(app.detail_scroll, 1);
    app.detail_scroll_up();
    assert_eq!(app.detail_scroll, 0);
    app.close_detail();
    assert!(!app.detail_open);

    // sem task selecionada não abre
    let dir2 = TmpDir::new("detail-empty");
    let mut empty = app_with(&dir2, &[]);
    empty.open_detail();
    assert!(!empty.detail_open);
}

#[test]
fn navegacao_respeita_limites() {
    let dir = TmpDir::new("nav");
    let mut app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "wip")]);
    assert_eq!(app.selected, 0);
    app.select_prev(); // não passa de 0
    assert_eq!(app.selected, 0);
    app.select_next();
    assert_eq!(app.selected, 1);
    app.select_next(); // não passa do fim
    assert_eq!(app.selected, 1);
}

#[test]
fn statusline_mostra_wip_count() {
    let dir = TmpDir::new("status");
    let app = app_with(&dir, &[("TASK-001", "wip"), ("TASK-002", "backlog")]);
    let s = app.statusline();
    assert!(s.contains("[Board]"));
    assert!(s.contains("▶ 1 play"));
    assert_eq!(app.wip_count(), 1);
}

#[test]
fn defer_cria_backlog_novo_e_refresh() {
    let dir = TmpDir::new("defer");
    let mut app = app_with(&dir, &[("TASK-001", "wip")]);
    let antes = app.tasks.len();
    app.defer("extrair parser de datas");
    assert_eq!(app.tasks.len(), antes + 1);
    assert!(app.status_msg.contains("deferred"));
}

// ingest agora é via LLM (claude -p) — a lógica determinística (parse do
// structured_output + create_stubs) é testada em crates/flows/tests/ingest.rs.
