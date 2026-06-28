//! Estado e lógica pura da TUI (testável sem terminal). O render fica em `tui`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use anyhow::{Context, Result};
use jaum_adapters::{ClaudeExecutor, Gh, Git, Session};

use crate::config::Config as GlobalConfig;
use jaum_core::{Status, Store, Task};
use jaum_flows::conflict::Conflict;
use jaum_flows::finish::Finish;
use jaum_flows::ingest::Ingest;
use jaum_flows::play::{Play, PlaySession};
use jaum_flows::review::{ConstraintVerdict, Review, ReviewReport};

/// Abas da TUI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Board,
    Session,
    Review,
    Docs,
}

impl Tab {
    pub fn all() -> [Tab; 4] {
        [Tab::Board, Tab::Session, Tab::Review, Tab::Docs]
    }
    pub fn title(self) -> &'static str {
        match self {
            Tab::Board => "Board",
            Tab::Session => "Session",
            Tab::Review => "Review",
            Tab::Docs => "Docs",
        }
    }
    pub fn index(self) -> usize {
        Tab::all().iter().position(|t| *t == self).unwrap()
    }
    pub fn from_index(i: usize) -> Tab {
        Tab::all()[i.min(3)]
    }
    pub fn next(self) -> Tab {
        Tab::from_index((self.index() + 1) % 4)
    }
    pub fn prev(self) -> Tab {
        Tab::from_index((self.index() + 3) % 4)
    }
}

/// Ordem canônica de exibição dos status no board.
pub const STATUS_ORDER: [Status; 5] = [
    Status::Wip,
    Status::Review,
    Status::Ready,
    Status::Backlog,
    Status::Merged,
];

pub fn status_label(s: Status) -> &'static str {
    match s {
        Status::Backlog => "backlog",
        Status::Ready => "ready",
        Status::Wip => "wip",
        Status::Review => "review",
        Status::Merged => "merged",
    }
}

fn status_rank(s: Status) -> usize {
    STATUS_ORDER
        .iter()
        .position(|x| *x == s)
        .unwrap_or(usize::MAX)
}

/// Ordena tasks para o board: por status (ordem canônica), depois por id.
pub fn sort_for_board(mut tasks: Vec<Task>) -> Vec<Task> {
    tasks.sort_by(|a, b| {
        status_rank(a.status)
            .cmp(&status_rank(b.status))
            .then_with(|| a.id.cmp(&b.id))
    });
    tasks
}

/// Metadados de uma sessão de play (para limpeza das worktrees no stop).
pub struct PlayMeta {
    pub id: String,
    pub worktrees: Vec<(String, PathBuf)>,
}

const HINT: &str = "j/k mover · Enter detalhes · h/l aba · p play · r review · f finish · i ingest · d defer · P projeto · q sair";

/// Estado completo da aplicação.
pub struct App {
    pub git: Git,
    pub gh: Gh,
    pub executor: ClaudeExecutor,

    /// Config global (todos os projetos) e o índice do projeto atual.
    pub config: GlobalConfig,
    pub current: usize,
    /// Derivados do projeto atual:
    pub store: Store,
    pub repos: HashMap<String, PathBuf>,
    pub docs_dir: PathBuf,
    pub work_dir: PathBuf,

    pub tab: Tab,
    pub tasks: Vec<Task>,
    pub selected: usize,
    pub overlaps: Vec<(String, String, String)>,
    pub status_msg: String,
    pub should_quit: bool,

    /// Sessão (PTY) viva — de play OU review read-only.
    pub session: Option<Session>,
    pub parser: Option<vt100::Parser>,
    pub pty_rx: Option<Receiver<Vec<u8>>>,
    /// Presente só quando a sessão é de play (tem worktrees a limpar).
    pub play_meta: Option<PlayMeta>,

    pub review_report: Option<ReviewReport>,

    /// Aba Session: prefixo (Ctrl+B) pendente para o próximo comando do jaum.
    pub pending_prefix: bool,
    /// `Some(buffer)` enquanto capturando texto (ex.: defer).
    pub input: Option<String>,

    /// Overlay de troca de projeto.
    pub project_picker: bool,
    pub picker_selected: usize,

    /// Overlay de detalhe da task selecionada.
    pub detail_open: bool,
    /// Scroll vertical do detalhe.
    pub detail_scroll: u16,
}

impl App {
    /// Cria o App a partir da config global, abrindo o projeto `current`.
    pub fn new(config: GlobalConfig, current: usize) -> Result<Self> {
        let project = config
            .projects
            .get(current)
            .context("índice de projeto inválido")?;
        let mut app = Self {
            git: Git::new(),
            gh: Gh::new(),
            executor: ClaudeExecutor::new(),
            store: Store::new(&project.backlog),
            repos: project.repo_map(),
            docs_dir: project.docs.clone(),
            work_dir: project.work_dir.clone(),
            current,
            config,
            tab: Tab::Board,
            tasks: Vec::new(),
            selected: 0,
            overlaps: Vec::new(),
            status_msg: HINT.into(),
            should_quit: false,
            session: None,
            parser: None,
            pty_rx: None,
            play_meta: None,
            review_report: None,
            pending_prefix: false,
            input: None,
            project_picker: false,
            picker_selected: 0,
            detail_open: false,
            detail_scroll: 0,
        };
        app.refresh()?;
        Ok(app)
    }

    // --- detalhe da task ---------------------------------------------------

    pub fn open_detail(&mut self) {
        if self.selected_task().is_some() {
            self.detail_open = true;
            self.detail_scroll = 0;
        }
    }
    pub fn close_detail(&mut self) {
        self.detail_open = false;
    }
    pub fn detail_scroll_down(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_add(1);
    }
    pub fn detail_scroll_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(1);
    }

    pub fn project_name(&self) -> &str {
        self.config
            .projects
            .get(self.current)
            .map(|p| p.name.as_str())
            .unwrap_or("?")
    }

    /// Troca para outro projeto (encerra sessão, recarrega store/repos/docs).
    pub fn load_project(&mut self, i: usize) {
        let Some(project) = self.config.projects.get(i).cloned() else {
            return;
        };
        self.stop_session();
        self.store = Store::new(&project.backlog);
        self.repos = project.repo_map();
        self.docs_dir = project.docs.clone();
        self.work_dir = project.work_dir.clone();
        self.current = i;
        self.selected = 0;
        self.tab = Tab::Board;
        self.status_msg = format!("projeto: {}", project.name);
        let _ = self.refresh();
    }

    // --- picker de projeto -------------------------------------------------

    pub fn open_picker(&mut self) {
        self.project_picker = true;
        self.picker_selected = self.current;
    }
    pub fn close_picker(&mut self) {
        self.project_picker = false;
    }
    pub fn picker_next(&mut self) {
        if !self.config.projects.is_empty() {
            self.picker_selected = (self.picker_selected + 1).min(self.config.projects.len() - 1);
        }
    }
    pub fn picker_prev(&mut self) {
        self.picker_selected = self.picker_selected.saturating_sub(1);
    }
    pub fn confirm_picker(&mut self) {
        let i = self.picker_selected;
        self.project_picker = false;
        if i != self.current {
            self.load_project(i);
        }
    }

    /// Recarrega tasks e overlaps do disco.
    pub fn refresh(&mut self) -> Result<()> {
        self.tasks = sort_for_board(self.store.list(None)?);
        if !self.tasks.is_empty() && self.selected >= self.tasks.len() {
            self.selected = self.tasks.len() - 1;
        }
        self.overlaps = Conflict::new(&self.store)
            .detect_overlap()
            .unwrap_or_default();
        Ok(())
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.tasks.get(self.selected)
    }

    pub fn select_next(&mut self) {
        if !self.tasks.is_empty() {
            self.selected = (self.selected + 1).min(self.tasks.len() - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.tasks.len().saturating_sub(1);
    }

    /// Nº de tasks em `wip` (badge ▶ N play).
    pub fn wip_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status == Status::Wip)
            .count()
    }

    /// Report de review da task (se houver `.review.md`). Não precisa de sessão.
    pub fn load_review(&self, id: &str) -> Option<ReviewReport> {
        let path = self.store.review_path(id);
        self.store
            .read_doc::<ReviewReport>(&path)
            .ok()
            .map(|(r, _)| r)
    }

    /// Badge ⚑ N na review: findings + constraints não-ok.
    pub fn review_badge(&self, id: &str) -> Option<usize> {
        let r = self.load_review(id)?;
        let bad = r.findings.len()
            + r.constraints
                .iter()
                .filter(|c| c.verdict != ConstraintVerdict::Ok)
                .count();
        Some(bad)
    }

    /// Linha de status: aba, task/branch selecionada, ▶ N play, ⚠ overlap.
    pub fn statusline(&self) -> String {
        let mut s = format!("[{}]", self.tab.title());
        if let Some(t) = self.selected_task() {
            s.push_str(&format!(" {}", t.id));
            if let Some(pr) = t.prs.first() {
                s.push_str(&format!(" {}", pr.branch));
            }
        }
        s.push_str(&format!(" · ▶ {} play", self.wip_count()));
        if let Some((a, b, repo)) = self.overlaps.first() {
            s.push_str(&format!(" · ⚠ overlap {repo} ({a}↔{b})"));
        }
        s
    }

    // --- ações (efeitos colaterais) ---------------------------------------

    pub fn play_selected(&mut self) {
        let Some(id) = self.selected_task().map(|t| t.id.clone()) else {
            self.status_msg = "nenhuma task selecionada".into();
            return;
        };
        let result = Play::new(
            &self.store,
            &self.git,
            &self.executor,
            &self.work_dir,
            self.repos.clone(),
        )
        .start(&id);
        match result {
            Ok(ps) => {
                let PlaySession {
                    id,
                    session,
                    worktrees,
                    ..
                } = ps;
                self.start_pty_pump(session);
                self.play_meta = Some(PlayMeta {
                    id: id.clone(),
                    worktrees,
                });
                self.tab = Tab::Session;
                self.status_msg = format!("play iniciado em {id}");
            }
            Err(e) => self.status_msg = format!("play falhou: {e}"),
        }
        let _ = self.refresh();
    }

    pub fn review_selected(&mut self) {
        let Some(id) = self.selected_task().map(|t| t.id.clone()) else {
            return;
        };
        self.review_report = self.load_review(&id);
        let result = Review::new(
            &self.store,
            &self.git,
            &self.executor,
            &self.docs_dir,
            self.repos.clone(),
        )
        .start(&id);
        match result {
            Ok(session) => {
                self.start_pty_pump(session);
                self.play_meta = None; // review não tem worktrees a limpar
                self.tab = Tab::Session;
                self.status_msg = format!("review read-only de {id}");
            }
            Err(e) => self.status_msg = format!("review falhou: {e}"),
        }
    }

    pub fn finish_selected(&mut self) {
        let Some(id) = self.selected_task().map(|t| t.id.clone()) else {
            return;
        };
        match Finish::new(&self.store, &self.gh).run(&id) {
            Ok(state) => {
                self.status_msg = format!("finish {id}: merge {state:?} (merge é comando seu)")
            }
            Err(e) => self.status_msg = format!("finish falhou: {e}"),
        }
        let _ = self.refresh();
    }

    /// Ingest mínimo: cria um stub para cada `RFC-*.md` em `docs/` ainda não
    /// referenciado por nenhuma task. Usa o `claude -p` (bloqueia a TUI por
    /// alguns segundos enquanto varre — para acompanhar o progresso, use
    /// `jaum ingest` no terminal).
    pub fn ingest(&mut self) {
        let root = self
            .store
            .root()
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let add_dirs: Vec<PathBuf> = self.repos.values().cloned().collect();
        let ingest = Ingest::new(&self.store, &self.executor, root, add_dirs);
        match ingest.run() {
            Ok(created) => self.status_msg = format!("ingest: {} stub(s) criados", created.len()),
            Err(e) => self.status_msg = format!("ingest falhou: {e}"),
        }
        let _ = self.refresh();
    }

    /// Registra escopo extra como deferred e cria um backlog novo.
    pub fn defer(&mut self, text: &str) {
        let Some(id) = self.selected_task().map(|t| t.id.clone()) else {
            return;
        };
        if text.trim().is_empty() {
            self.status_msg = "defer cancelado (texto vazio)".into();
            return;
        }
        match self.store.add_deferred(&id, text) {
            Ok(new) => self.status_msg = format!("deferred de {id} -> {}", new.id),
            Err(e) => self.status_msg = format!("defer falhou: {e}"),
        }
        let _ = self.refresh();
    }

    pub fn stop_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            let _ = s.kill();
        }
        if let Some(meta) = self.play_meta.take() {
            for (repo, _) in &meta.worktrees {
                if let Ok(task) = self.store.get(&meta.id)
                    && let Some(link) = task.prs.iter().find(|p| &p.repo == repo)
                    && let Some(repo_path) = self.repos.get(repo)
                {
                    let _ = self.git.worktree_remove(repo_path, &link.branch);
                }
            }
        }
        self.parser = None;
        self.pty_rx = None;
    }

    /// Aplica os bytes pendentes do PTY no parser vt100.
    pub fn drain_pty(&mut self) {
        if let (Some(rx), Some(parser)) = (&self.pty_rx, &mut self.parser) {
            while let Ok(bytes) = rx.try_recv() {
                parser.process(&bytes);
            }
        }
    }

    fn start_pty_pump(&mut self, session: Session) {
        use std::sync::mpsc::channel;
        use std::thread;

        let mut parser = vt100::Parser::new(40, 120, 2000);
        parser.screen_mut().set_size(40, 120);
        if let Ok(mut reader) = session.reader() {
            let (tx, rx) = channel::<Vec<u8>>();
            thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            self.pty_rx = Some(rx);
        }
        self.parser = Some(parser);
        self.session = Some(session);
    }
}
