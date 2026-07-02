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
        root: dir.0.clone(),
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

#[test]
fn add_convention_anexa_no_arquivo_e_recarrega() {
    let dir = TmpDir::new("conv");
    let mut app = app_with(&dir, &[]);
    app.add_convention("não referenciar nº de RFC em comentários");
    assert!(app.conventions.contains("não referenciar nº de RFC"));
    let no_disco = fs::read_to_string(&app.conventions_path).unwrap();
    assert!(no_disco.contains("- não referenciar nº de RFC em comentários"));
}

#[test]
fn new_task_quick_cria_backlog_com_objetivo() {
    let dir = TmpDir::new("newtask");
    let mut app = app_with(&dir, &[]);
    app.new_task_quick("analisar o resto do projeto procurando refs de RFC");
    assert_eq!(app.tasks.len(), 1);
    assert!(app.tasks[0].body.contains("analisar o resto do projeto"));
    assert!(app.status_msg.contains("task criada"));
}

fn cat_session() -> jaum_adapters::Session {
    use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor};
    ClaudeExecutor::with_bin("cat")
        .spawn_interactive("", &ExecFlags::default())
        .unwrap()
}

#[test]
fn multi_sessao_navega_e_encerra() {
    use app::SessionKind;
    let dir = TmpDir::new("multi");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    assert!(a.sessions.is_empty());

    a.open_session(
        SessionKind::Play,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-1".into(),
        dir.0.clone(),
    );
    a.open_session(
        SessionKind::Review,
        Some("TASK-002".into()),
        cat_session(),
        Vec::new(),
        "uuid-2".into(),
        dir.0.clone(),
    );
    assert_eq!(a.sessions.len(), 2);
    // a mais nova (TASK-002) vai pro topo e segue selecionada
    assert_eq!(a.session_selected, 0);
    assert_eq!(a.selected_session().unwrap().name(), "review · TASK-002");
    assert_eq!(a.tab, Tab::Session);

    a.session_next(); // 0 -> 1
    assert_eq!(a.session_selected, 1);
    a.session_prev(); // 1 -> 0
    assert_eq!(a.session_selected, 0);

    // finish: marca como concluída mas MANTÉM na lista (histórico)
    a.finish_selected_session();
    assert_eq!(a.sessions.len(), 2);
    assert!(a.selected_session().unwrap().finished);
    assert!(a.status_msg.contains("finalizada"));

    a.close_selected_session(); // remove a 2ª, clampa a seleção
    assert_eq!(a.sessions.len(), 1);
    assert_eq!(a.session_selected, 0);
    assert_eq!(a.selected_session().unwrap().name(), "play · TASK-001");

    a.stop_all_sessions();
    assert!(a.sessions.is_empty());
}

#[test]
fn sessions_mais_recente_no_topo() {
    use app::SessionKind;
    let dir = TmpDir::new("sort");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    for uuid in ["s1", "s2", "s3"] {
        a.open_session(
            SessionKind::Play,
            Some("TASK-001".into()),
            cat_session(),
            Vec::new(),
            uuid.into(),
            dir.0.clone(),
        );
    }
    // a última aberta (s3) fica no topo
    assert_eq!(a.sessions[0].claude_session_id, "s3");

    // atividade recente numa sessão antiga (s1) a leva pro topo
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    a.sessions
        .iter_mut()
        .find(|e| e.claude_session_id == "s1")
        .unwrap()
        .last_activity = later;
    a.sort_sessions();
    assert_eq!(a.sessions[0].claude_session_id, "s1");
}

#[test]
fn play_selected_foca_sessao_viva_existente_sem_duplicar() {
    use app::SessionKind;
    let dir = TmpDir::new("play-dup");
    let mut a = app_with(&dir, &[("TASK-001", "wip")]);
    // já existe uma sessão de play viva para a task selecionada
    a.open_session(
        SessionKind::Play,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-live".into(),
        dir.0.clone(),
    );
    a.tab = Tab::Board;
    assert_eq!(a.sessions.len(), 1);

    a.play_selected();
    // não duplicou: focou a sessão existente
    assert_eq!(a.sessions.len(), 1);
    assert_eq!(a.tab, Tab::Session);
    assert!(a.status_msg.contains("já está aberto"), "msg: {}", a.status_msg);
}

#[test]
fn sessao_finalizada_nao_conta_como_viva() {
    use app::SessionKind;
    let dir = TmpDir::new("dead-session");
    // task NÃO-wip: a única razão de ser "ativa" seria a sessão viva.
    let mut a = app_with(&dir, &[("TASK-001", "backlog")]);
    a.open_session(
        SessionKind::Play,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-dead".into(),
        dir.0.clone(),
    );
    assert!(a.sessions[0].is_live());
    assert!(a.active_task_ids().contains(&"TASK-001".to_string()));

    // simula o claude tendo saído (Ctrl+C/Ctrl+D): o drain marcaria `finished`.
    a.sessions[0].finished = true;
    assert!(
        !a.sessions[0].is_live(),
        "sessão finalizada (processo morto) não é viva, mesmo com session=Some"
    );
    // e não conta mais como task ativa
    assert!(!a.active_task_ids().contains(&"TASK-001".to_string()));
}

fn write_review(dir: &TmpDir, id: &str, clean: bool) {
    let body = if clean {
        format!("---\ntask: {id}\nfindings: []\nconstraints: []\n---\nok\n")
    } else {
        format!("---\ntask: {id}\nfindings:\n  - file: src/x.rs\n    message: bug\nconstraints: []\n---\nbug\n")
    };
    fs::write(dir.0.join(format!(".backlog/{id}.review.md")), body).unwrap();
}

#[test]
fn review_tab_lista_so_tasks_com_review_e_navega_independente_do_board() {
    let dir = TmpDir::new("review-list");
    let mut a = app_with(&dir, &[("TASK-001", "review"), ("TASK-002", "review"), ("TASK-003", "review")]);
    // só 001 e 003 têm review gravado
    write_review(&dir, "TASK-001", true);
    write_review(&dir, "TASK-003", false);
    a.refresh().unwrap();

    assert_eq!(a.review_ids.len(), 2, "só as tasks com .review.md entram");
    assert!(a.review_ids.contains(&"TASK-001".to_string()));
    assert!(a.review_ids.contains(&"TASK-003".to_string()));
    assert!(!a.review_ids.contains(&"TASK-002".to_string()));

    // na aba Review, j/k movem o cursor da lista, NÃO a seleção do Board
    a.tab = Tab::Review;
    let board_before = a.selected;
    assert_eq!(a.review_selected, 0);
    assert_eq!(a.target_task_id().as_deref(), Some(a.review_ids[0].as_str()));

    a.review_next();
    assert_eq!(a.review_selected, 1);
    assert_eq!(a.selected, board_before, "Board não deve mexer");
    assert_eq!(a.target_task_id().as_deref(), Some(a.review_ids[1].as_str()));

    a.review_prev();
    assert_eq!(a.review_selected, 0);
}

#[test]
fn handoff_selected_envia_findings_ao_play() {
    use app::SessionKind;
    let dir = TmpDir::new("handoff");
    let mut a = app_with(&dir, &[("TASK-001", "review")]);
    // grava um review SUJO (1 finding + 1 constraint reprovada)
    fs::write(
        dir.0.join(".backlog/TASK-001.review.md"),
        "---\ntask: TASK-001\nfindings:\n  - file: src/x.rs\n    message: bug\nconstraints:\n  - text: regra\n    verdict: reprovado\n---\nbody\n",
    )
    .unwrap();
    a.refresh().unwrap();
    // já existe uma sessão de play viva para a task
    a.open_session(
        SessionKind::Play,
        Some("TASK-001".into()),
        cat_session(),
        Vec::new(),
        "uuid-h".into(),
        dir.0.clone(),
    );

    a.handoff_selected();
    assert_eq!(a.tab, Tab::Session);
    assert!(a.status_msg.contains("enviados"), "msg: {}", a.status_msg);
}

#[test]
fn handoff_selected_sem_review_avisa() {
    let dir = TmpDir::new("handoff-none");
    let mut a = app_with(&dir, &[("TASK-001", "review")]);
    a.handoff_selected();
    assert!(a.status_msg.contains("rode o review"));
}

#[test]
fn paralelismo_badge_relativo_a_task_ativa() {
    let dir = TmpDir::new("parallel");
    let work = dir.0.join(".jaum");
    fs::create_dir_all(&work).unwrap();
    // conflito declarado entre 001 e 002 (no repo org/x)
    fs::write(
        work.join("parallel.json"),
        r#"{"conflicts":[{"a":"TASK-001","b":"TASK-002","repo":"org/x","reason":"ambas editam src/x.rs"}]}"#,
    )
    .unwrap();
    // 001 está wip (ativa); 002 e 003 paradas
    let mut a = app_with(
        &dir,
        &[("TASK-001", "wip"), ("TASK-002", "backlog"), ("TASK-003", "backlog")],
    );
    a.refresh().unwrap();
    assert!(a.parallel.is_some(), "parallel.json deve ter sido carregado");

    // 002 conflita com a ativa 001
    let c = a.parallel_conflict_with_active("TASK-002");
    assert!(c.is_some());
    let (other, repo, _reason) = c.unwrap();
    assert_eq!(other, "TASK-001");
    assert_eq!(repo, "org/x");
    assert!(!a.is_parallel_safe("TASK-002"), "002 conflita, não é safe");

    // 003 não conflita com a ativa -> safe para paralelo
    assert!(a.parallel_conflict_with_active("TASK-003").is_none());
    assert!(a.is_parallel_safe("TASK-003"));

    // sem análise carregada, nada é marcado
    a.parallel = None;
    assert!(a.parallel_conflict_with_active("TASK-002").is_none());
    assert!(!a.is_parallel_safe("TASK-003"));
}

#[test]
fn paralelismo_sem_task_ativa_nao_marca_safe() {
    let dir = TmpDir::new("parallel-idle");
    let work = dir.0.join(".jaum");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("parallel.json"), r#"{"conflicts":[]}"#).unwrap();
    let mut a = app_with(&dir, &[("TASK-001", "backlog"), ("TASK-002", "backlog")]);
    a.refresh().unwrap();
    // nada ativo -> não há com quem paralelizar -> não é "safe"
    assert!(!a.is_parallel_safe("TASK-001"));
    assert!(a.active_task_ids().is_empty());
}

#[test]
fn session_record_roundtrip_serde() {
    use app::{SessionKind, SessionRecord};
    let rec = SessionRecord {
        kind: SessionKind::Play,
        task: Some("TASK-001".into()),
        claude_session_id: "abc-123".into(),
        cwd: PathBuf::from("/tmp/wt"),
        worktrees: vec![("org/x".into(), PathBuf::from("/tmp/wt"))],
        created_ms: 1_700_000_000_000,
        last_activity_ms: 1_700_000_005_000,
        finished: false,
    };
    let json = serde_json::to_string(&rec).unwrap();
    // kind serializa em lowercase
    assert!(json.contains("\"play\""), "json: {json}");
    let back: SessionRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back.kind, SessionKind::Play);
    assert_eq!(back.claude_session_id, "abc-123");
    assert_eq!(back.cwd, PathBuf::from("/tmp/wt"));
    assert_eq!(back.created_ms, 1_700_000_000_000);
    assert!(!back.finished);
}

#[test]
fn rehydrate_traz_finalizadas_e_cwd_ausente_como_historico() {
    use app::{SessionKind, SessionRecord};
    let dir = TmpDir::new("rehydrate");
    let work = dir.0.join(".jaum");
    fs::create_dir_all(&work).unwrap();

    // 1) finalizada (vira histórico mesmo com cwd existente)
    // 2) viva mas cwd inexistente (não resumível -> histórico)
    let recs = vec![
        SessionRecord {
            kind: SessionKind::Setup,
            task: None,
            claude_session_id: "s-fin".into(),
            cwd: dir.0.clone(),
            worktrees: Vec::new(),
            created_ms: 1_700_000_000_000,
            last_activity_ms: 1_700_000_001_000,
            finished: true,
        },
        SessionRecord {
            kind: SessionKind::Play,
            task: Some("TASK-001".into()),
            claude_session_id: "s-gone".into(),
            cwd: dir.0.join("worktree-que-sumiu"),
            worktrees: Vec::new(),
            created_ms: 1_700_000_002_000,
            last_activity_ms: 1_700_000_003_000,
            finished: false,
        },
    ];
    fs::write(
        work.join("sessions.json"),
        serde_json::to_string(&recs).unwrap(),
    )
    .unwrap();

    // App::new reidrata no boot
    let a = app_with(&dir, &[("TASK-001", "wip")]);
    assert_eq!(a.sessions.len(), 2, "ambas devem aparecer na lista");
    // nenhuma é viva: finalizada e cwd-ausente caem para histórico (sem PTY)
    assert!(a.sessions.iter().all(|e| !e.is_live()));
    assert!(a.sessions.iter().all(|e| e.finished));
    assert_eq!(a.sessions[0].kind, SessionKind::Setup);
    assert_eq!(a.sessions[1].claude_session_id, "s-gone");
}

// ingest agora é via LLM (claude -p) — a lógica determinística (parse do
// structured_output + create_stubs) é testada em crates/flows/tests/ingest.rs.
