//! Estado e lógica pura da TUI (testável sem terminal). O render fica em `tui`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use jaum_adapters::{ClaudeExecutor, Gh, Git, Session};
use serde::{Deserialize, Serialize};

use crate::config::Config as GlobalConfig;
use jaum_core::{Status, Store, Task};
use jaum_flows::conflict::Conflict;
use jaum_flows::finish::Finish;
use jaum_flows::ingest::Ingest;
use jaum_flows::parallel::{Parallel, ParallelReport};
use jaum_flows::play::{Play, PlaySession};
use jaum_flows::review::{ConstraintVerdict, Review, ReviewReport};
use jaum_flows::setup::{Setup, branch_leaks_id, is_template};

/// O que falta no setup obrigatório do projeto (validado no init/abertura). É o
/// jaum (não o prompt) que impõe estas invariantes.
#[derive(Debug, Default)]
pub struct SetupNeeds {
    /// Tasks no backlog sem repo/branch vinculado.
    pub unlinked: Vec<String>,
    /// Tasks cujo branch vaza o id interno do jaum (ex.: `feat/task-001`).
    pub leaky_branches: Vec<String>,
    /// `conventions.md` ainda no template/vazio.
    pub conventions_template: bool,
    /// `setup.md` (mapeamento repo↔área) ausente.
    pub mapping_missing: bool,
}

impl SetupNeeds {
    pub fn any(&self) -> bool {
        !self.unlinked.is_empty()
            || !self.leaky_branches.is_empty()
            || self.conventions_template
            || self.mapping_missing
    }
}

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

/// Lista (recursiva) os caminhos relativos de todos os `.md` sob `dir`.
pub fn list_docs(dir: &std::path::Path) -> Vec<String> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if p.extension().is_some_and(|e| e == "md")
                && let Ok(rel) = p.strip_prefix(base)
            {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Tipo de uma sessão viva (rótulo + cor na lista).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Play,
    Review,
    Setup,
}

impl SessionKind {
    pub fn label(self) -> &'static str {
        match self {
            SessionKind::Play => "play",
            SessionKind::Review => "review",
            SessionKind::Setup => "setup",
        }
    }
}

/// Epoch em milissegundos de um `SystemTime` (0 se antes da época).
fn epoch_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// `SystemTime` a partir de epoch em milissegundos.
fn from_epoch_ms(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// Registro persistido de uma sessão (sobrevive ao shutdown do daemon). Não
/// guarda o PTY — só o suficiente para resumir (`claude --resume`) ou exibir
/// como histórico.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub kind: SessionKind,
    pub task: Option<String>,
    /// UUID da sessão do claude (`--session-id`/`--resume`).
    pub claude_session_id: String,
    /// cwd de origem (a worktree do play, o repo do review, o home do setup).
    pub cwd: PathBuf,
    pub worktrees: Vec<(String, PathBuf)>,
    pub created_ms: u64,
    pub last_activity_ms: u64,
    pub finished: bool,
}

/// Uma sessão e seus metadados, para rodar várias em paralelo. Quando viva tem
/// PTY (`session`/`rx`) e parser vt100 alimentado por uma thread leitora; quando
/// histórico (restaurada do disco e não resumível), `session`/`rx` são `None`.
pub struct SessionEntry {
    pub kind: SessionKind,
    /// Task vinculada (None para o setup).
    pub task: Option<String>,
    pub session: Option<Session>,
    pub parser: vt100::Parser,
    pub rx: Option<Receiver<Vec<u8>>>,
    /// Worktrees a limpar no encerramento (só play).
    pub worktrees: Vec<(String, PathBuf)>,
    /// UUID da sessão do claude, para resumir após restart.
    pub claude_session_id: String,
    /// cwd de origem (preservado para o resume achar a conversa).
    pub cwd: PathBuf,
    pub created: SystemTime,
    pub last_activity: SystemTime,
    pub finished: bool,
    /// Sequência monotônica de criação. Desempata a ordenação por atividade
    /// (mais novo no topo) quando os timestamps coincidem.
    pub seq: u64,
}

/// Contador global de criação de sessões (desempate determinístico da ordenação).
static SESSION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn next_session_seq() -> u64 {
    SESSION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl SessionEntry {
    /// Cria a entrada VIVA e dispara a thread que bombeia o PTY para o parser.
    fn spawn(
        kind: SessionKind,
        task: Option<String>,
        session: Session,
        worktrees: Vec<(String, PathBuf)>,
        claude_session_id: String,
        cwd: PathBuf,
    ) -> Self {
        use std::sync::mpsc::channel;
        use std::thread;

        let mut parser = vt100::Parser::new(40, 120, 2000);
        parser.screen_mut().set_size(40, 120);
        let (tx, rx) = channel::<Vec<u8>>();
        let mut rx = Some(rx);
        if let Ok(mut reader) = session.reader() {
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
        } else {
            rx = None;
        }
        let now = SystemTime::now();
        Self {
            kind,
            task,
            session: Some(session),
            parser,
            rx,
            worktrees,
            claude_session_id,
            cwd,
            created: now,
            last_activity: now,
            finished: false,
            seq: next_session_seq(),
        }
    }

    /// Entrada de HISTÓRICO (sem PTY): restaurada do disco quando a sessão não é
    /// resumível (finalizada, cwd sumiu ou o resume falhou).
    fn history(rec: &SessionRecord) -> Self {
        let mut parser = vt100::Parser::new(40, 120, 2000);
        parser.screen_mut().set_size(40, 120);
        Self {
            kind: rec.kind,
            task: rec.task.clone(),
            session: None,
            parser,
            rx: None,
            worktrees: rec.worktrees.clone(),
            claude_session_id: rec.claude_session_id.clone(),
            cwd: rec.cwd.clone(),
            created: from_epoch_ms(rec.created_ms),
            last_activity: from_epoch_ms(rec.last_activity_ms),
            finished: true,
            seq: next_session_seq(),
        }
    }

    /// Snapshot serializável da sessão.
    fn to_record(&self) -> SessionRecord {
        SessionRecord {
            kind: self.kind,
            task: self.task.clone(),
            claude_session_id: self.claude_session_id.clone(),
            cwd: self.cwd.clone(),
            worktrees: self.worktrees.clone(),
            created_ms: epoch_ms(self.created),
            last_activity_ms: epoch_ms(self.last_activity),
            finished: self.finished,
        }
    }

    /// Tem PTY vivo? Falso para histórico (sem `session`) E para sessões cujo
    /// processo já saiu (ex.: o claude encerrou com Ctrl+C/Ctrl+D) — marcadas
    /// `finished` no `drain`. Assim o play volta a iniciar uma sessão nova.
    pub fn is_live(&self) -> bool {
        self.session.is_some() && !self.finished
    }

    /// Nome exibido na lista: `play · TASK-001`, `setup`, etc.
    pub fn name(&self) -> String {
        match &self.task {
            Some(t) => format!("{} · {t}", self.kind.label()),
            None => self.kind.label().to_string(),
        }
    }

    /// Drena os bytes pendentes do PTY; marca `finished` no EOF. No-op para
    /// entradas de histórico (sem `rx`).
    fn drain(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = &self.rx else { return };
        loop {
            match rx.try_recv() {
                Ok(bytes) => {
                    self.parser.process(&bytes);
                    self.last_activity = SystemTime::now();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.finished = true;
                    break;
                }
            }
        }
        // detecção proativa: o processo pode ter saído (Ctrl+C/Ctrl+D no claude)
        // antes do EOF do canal propagar. `try_wait` no filho é autoritativo.
        if !self.finished
            && let Some(s) = &mut self.session
            && matches!(s.try_wait(), Ok(Some(_)))
        {
            self.finished = true;
        }
    }
}

/// O que o input de texto está capturando (despachado no Enter).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Defer,
    Convention,
    NewTask,
    NewTaskClaude,
    InitPath,
}

/// Tipo de job assíncrono (define o que fazer quando ele termina).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Ingest,
    Capture,
    Init,
    Review,
    Parallel,
}

/// Mensagem de um job em background para a UI.
pub enum JobMsg {
    /// Linha de log ao vivo.
    Log(String),
    /// Encerramento. `Ok` = nome do projeto (Init) ou frase de sucesso; `Err` =
    /// mensagem de falha.
    Done(Result<String, String>),
}

/// Um job assíncrono (ingest/captura/init) com seus logs ao vivo.
pub struct Job {
    pub kind: JobKind,
    pub title: String,
    pub logs: Vec<String>,
    pub rx: Receiver<JobMsg>,
    pub finished: bool,
    /// Offset de scroll (linhas, pós-wrap). Só vale quando `follow` é false.
    pub scroll: u16,
    /// Acompanha o fim ao vivo; desliga quando o usuário sobe pra ler.
    pub follow: bool,
}

/// Expande `~`/`~/...` para o HOME (a TUI recebe caminhos digitados pelo usuário).
fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

const HINT: &str = "jaum";

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
    /// Boas práticas do projeto (conteúdo do conventions.md) + caminho.
    pub conventions: String,
    pub conventions_path: PathBuf,

    pub tab: Tab,
    pub tasks: Vec<Task>,
    pub selected: usize,
    pub overlaps: Vec<(String, String, String)>,
    /// Mensagem da última interação. Não vai pro rodapé: vira um toast temporário.
    pub status_msg: String,
    /// Quando o toast atual começou (None = nenhum) e o texto já mostrado.
    pub toast_started: Option<Instant>,
    pub toast_shown: String,
    /// Última releitura do estado em disco (backlog/conventions), p/ pegar edições
    /// externas — ex.: o que a sessão de setup acabou de gravar.
    pub last_reload: Instant,
    /// File watcher da pasta do projeto: dispara reload imediato quando uma sessão
    /// (setup/play) grava algo. `watcher` é guardado só para mantê-lo vivo.
    watcher: Option<notify::RecommendedWatcher>,
    watch_rx: Option<Receiver<()>>,
    pub should_quit: bool,

    /// Sessões (PTYs) vivas, rodando em paralelo. `session_selected` é a exibida
    /// na aba Session e que recebe o input.
    pub sessions: Vec<SessionEntry>,
    pub session_selected: usize,

    pub review_report: Option<ReviewReport>,

    /// Aba Review: ids das tasks que têm `.review.md` (recomputado em `refresh`) e
    /// o cursor da lista, independente da seleção do Board.
    pub review_ids: Vec<String>,
    pub review_selected: usize,

    /// Última análise de paralelismo (carregada de `work_dir/parallel.json`).
    pub parallel: Option<ParallelReport>,

    /// Aba Session: prefixo (Ctrl+B) pendente para o próximo comando do jaum.
    pub pending_prefix: bool,
    /// `Some((tipo, buffer))` enquanto capturando texto (defer/convenção/task).
    pub input: Option<(InputKind, String)>,

    /// Overlay de troca de projeto.
    pub project_picker: bool,
    pub picker_selected: usize,

    /// Overlay de detalhe da task selecionada.
    pub detail_open: bool,
    /// Scroll vertical do detalhe.
    pub detail_scroll: u16,

    /// Docs (caminhos relativos sob docs_dir) e navegação/visualização.
    pub docs: Vec<String>,
    pub docs_selected: usize,
    pub doc_open: bool,
    pub doc_scroll: u16,

    /// Pedido de abrir o `conventions.md` no `$EDITOR` (tratado no event loop).
    pub edit_request: bool,

    /// Job assíncrono em andamento (ingest/captura/init) com logs ao vivo.
    pub job: Option<Job>,
    /// Overlay de logs visível (separado do job: o job segue em background).
    pub job_overlay: bool,
}

impl App {
    /// Cria o App a partir da config global, abrindo o projeto `current`.
    pub fn new(config: GlobalConfig, current: usize) -> Result<Self> {
        let project = config
            .projects
            .get(current)
            .context("índice de projeto inválido")?;
        let conventions_path = project.conventions_path();
        let conventions = std::fs::read_to_string(&conventions_path).unwrap_or_default();
        let mut app = Self {
            git: Git::new(),
            gh: Gh::new(),
            executor: ClaudeExecutor::new(),
            store: Store::new(&project.backlog),
            repos: project.repo_map(),
            docs_dir: project.docs.clone(),
            work_dir: project.work_dir.clone(),
            conventions,
            conventions_path,
            current,
            config,
            tab: Tab::Board,
            tasks: Vec::new(),
            selected: 0,
            overlaps: Vec::new(),
            status_msg: HINT.into(),
            toast_started: None,
            toast_shown: HINT.into(),
            last_reload: Instant::now(),
            watcher: None,
            watch_rx: None,
            should_quit: false,
            sessions: Vec::new(),
            session_selected: 0,
            review_report: None,
            review_ids: Vec::new(),
            review_selected: 0,
            parallel: None,
            pending_prefix: false,
            input: None,
            project_picker: false,
            picker_selected: 0,
            detail_open: false,
            detail_scroll: 0,
            docs: Vec::new(),
            docs_selected: 0,
            doc_open: false,
            doc_scroll: 0,
            edit_request: false,
            job: None,
            job_overlay: false,
        };
        app.refresh()?;
        app.rehydrate_sessions();
        app.start_watch();
        Ok(app)
    }

    /// Reidrata as sessões persistidas no boot: as vivas (não finalizadas, com a
    /// worktree/cwd ainda no disco) voltam resumidas via `claude --resume`; as
    /// demais entram como histórico (sem PTY). Nunca derruba o boot.
    pub(crate) fn rehydrate_sessions(&mut self) {
        for rec in self.load_session_records() {
            let entry = self.rehydrate_one(rec);
            self.sessions.push(entry);
        }
        self.session_selected = 0;
    }

    /// Constrói uma `SessionEntry` a partir de um registro: resume quando
    /// possível, senão vira histórico.
    fn rehydrate_one(&self, rec: SessionRecord) -> SessionEntry {
        // não resumível: já finalizada, ou o cwd (worktree/repo) sumiu.
        if rec.finished || !rec.cwd.exists() {
            return SessionEntry::history(&rec);
        }
        match self.resume_session(&rec) {
            Ok(session) => {
                let mut e = SessionEntry::spawn(
                    rec.kind,
                    rec.task.clone(),
                    session,
                    rec.worktrees.clone(),
                    rec.claude_session_id.clone(),
                    rec.cwd.clone(),
                );
                // preserva os tempos originais (não reinicia o relógio no resume).
                e.created = from_epoch_ms(rec.created_ms);
                e.last_activity = from_epoch_ms(rec.last_activity_ms);
                e
            }
            // resume falhou (cwd inválido, claude indisponível): cai pra histórico.
            Err(_) => SessionEntry::history(&rec),
        }
    }

    /// Relança o claude com `--resume` para o registro, conforme o tipo.
    fn resume_session(&self, rec: &SessionRecord) -> Result<Session> {
        match rec.kind {
            SessionKind::Play => {
                let id = rec.task.as_deref().context("registro de play sem task")?;
                Play::new(
                    &self.store,
                    &self.git,
                    &self.executor,
                    &self.work_dir,
                    self.repos.clone(),
                    self.conventions.clone(),
                )
                .resume(id, &rec.claude_session_id, &rec.cwd)
            }
            SessionKind::Review => {
                let id = rec.task.as_deref().context("registro de review sem task")?;
                Review::new(
                    &self.store,
                    &self.git,
                    &self.gh,
                    &self.executor,
                    &self.docs_dir,
                    self.repos.clone(),
                    self.conventions.clone(),
                )
                .resume(id, &rec.claude_session_id, &rec.cwd)
            }
            SessionKind::Setup => Setup::new(
                &self.store,
                &self.executor,
                self.home(),
                self.repos.clone(),
                self.conventions.clone(),
            )
            .resume(&rec.claude_session_id),
        }
    }

    /// (Re)inicia o file watcher na pasta do projeto atual. Qualquer escrita ali
    /// (o setup/play editando backlog/docs/conventions) dispara um reload imediato.
    fn start_watch(&mut self) {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = channel::<()>();
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = tx.send(());
            }
        }) {
            Ok(mut w) => {
                let _ = w.watch(&self.home(), RecursiveMode::Recursive);
                self.watcher = Some(w);
                self.watch_rx = Some(rx);
            }
            Err(_) => {
                self.watcher = None;
                self.watch_rx = None;
            }
        }
    }

    pub fn request_edit_conventions(&mut self) {
        self.edit_request = true;
    }

    pub fn reload_conventions(&mut self) {
        self.conventions = std::fs::read_to_string(&self.conventions_path).unwrap_or_default();
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
        self.stop_all_sessions();
        self.store = Store::new(&project.backlog);
        self.repos = project.repo_map();
        self.docs_dir = project.docs.clone();
        self.work_dir = project.work_dir.clone();
        self.conventions_path = project.conventions_path();
        self.conventions = std::fs::read_to_string(&self.conventions_path).unwrap_or_default();
        self.current = i;
        self.selected = 0;
        self.tab = Tab::Board;
        self.status_msg = format!("projeto: {}", project.name);
        let _ = self.refresh();
        self.rehydrate_sessions(); // sessões persistidas do novo projeto
        self.start_watch(); // observa a pasta do novo projeto
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

    /// Recarrega tasks, overlaps e a lista de docs do disco.
    pub fn refresh(&mut self) -> Result<()> {
        self.tasks = sort_for_board(self.store.list(None)?);
        if !self.tasks.is_empty() && self.selected >= self.tasks.len() {
            self.selected = self.tasks.len() - 1;
        }
        self.overlaps = Conflict::new(&self.store)
            .detect_overlap()
            .unwrap_or_default();
        // aba Review: tasks que já têm `.review.md` (independente do Board).
        self.review_ids = self
            .tasks
            .iter()
            .filter(|t| self.review_badge(&t.id).is_some())
            .map(|t| t.id.clone())
            .collect();
        if self.review_selected >= self.review_ids.len() {
            self.review_selected = self.review_ids.len().saturating_sub(1);
        }
        // análise de paralelismo persistida (best-effort; ausente = sem badges).
        self.parallel = std::fs::read_to_string(self.parallel_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        self.docs = list_docs(&self.docs_dir);
        if !self.docs.is_empty() && self.docs_selected >= self.docs.len() {
            self.docs_selected = self.docs.len() - 1;
        }
        Ok(())
    }

    // --- aba Review (lista de reviews) -------------------------------------

    /// Task sob o cursor da aba Review (a que está à mostra no detalhe).
    pub fn review_tab_task(&self) -> Option<String> {
        self.review_ids.get(self.review_selected).cloned()
    }

    pub fn review_next(&mut self) {
        if !self.review_ids.is_empty() {
            self.review_selected = (self.review_selected + 1).min(self.review_ids.len() - 1);
        }
    }

    pub fn review_prev(&mut self) {
        self.review_selected = self.review_selected.saturating_sub(1);
    }

    // --- paralelismo (badges no Board) -------------------------------------

    /// Arquivo da análise de paralelismo persistida.
    fn parallel_file(&self) -> PathBuf {
        self.work_dir.join("parallel.json")
    }

    /// Tasks "ativas" (referência do paralelismo): com sessão de play viva OU em
    /// status `wip`.
    pub fn active_task_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|e| e.is_live() && e.kind == SessionKind::Play)
            .filter_map(|e| e.task.clone())
            .collect();
        for t in &self.tasks {
            if t.status == Status::Wip && !ids.contains(&t.id) {
                ids.push(t.id.clone());
            }
        }
        ids
    }

    /// Se a task colide com alguma task ATIVA, devolve `(outra, repo, motivo)`.
    pub fn parallel_conflict_with_active(&self, id: &str) -> Option<(String, String, String)> {
        let report = self.parallel.as_ref()?;
        for other in self.active_task_ids() {
            if other == id {
                continue;
            }
            if let Some(c) = report.conflict_between(id, &other) {
                return Some((other, c.repo.clone(), c.reason.clone()));
            }
        }
        None
    }

    /// `true` se há análise, há outra task ativa, e esta não colide com nenhuma —
    /// ou seja, pode ser iniciada em paralelo com segurança.
    pub fn is_parallel_safe(&self, id: &str) -> bool {
        if self.parallel.is_none() {
            return false;
        }
        let has_other_active = self.active_task_ids().iter().any(|a| a != id);
        has_other_active && self.parallel_conflict_with_active(id).is_none()
    }

    // --- docs (aba Docs) ---------------------------------------------------

    pub fn docs_next(&mut self) {
        if !self.docs.is_empty() {
            self.docs_selected = (self.docs_selected + 1).min(self.docs.len() - 1);
        }
    }
    pub fn docs_prev(&mut self) {
        self.docs_selected = self.docs_selected.saturating_sub(1);
    }
    pub fn open_doc(&mut self) {
        if self.docs.get(self.docs_selected).is_none() {
            return;
        }
        // o conteúdo é lido fresh a cada frame no render (reflete edições externas).
        self.doc_open = true;
        self.doc_scroll = 0;
    }
    pub fn close_doc(&mut self) {
        self.doc_open = false;
    }
    pub fn doc_scroll_down(&mut self) {
        self.doc_scroll = self.doc_scroll.saturating_add(1);
    }
    pub fn doc_scroll_up(&mut self) {
        self.doc_scroll = self.doc_scroll.saturating_sub(1);
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.tasks.get(self.selected)
    }

    /// Id da task ALVO das ações (play/review/handoff/finish): a que está à mostra.
    /// Na aba Review é o cursor da lista de reviews; nas demais, a seleção do Board.
    pub fn target_task_id(&self) -> Option<String> {
        if self.tab == Tab::Review
            && let Some(id) = self.review_tab_task()
        {
            return Some(id);
        }
        self.selected_task().map(|t| t.id.clone())
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

    // --- setup do projeto -------------------------------------------------

    /// Pasta externa do projeto (`~/jaum/<proj>`), derivada do conventions_path.
    fn home(&self) -> PathBuf {
        self.conventions_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default()
    }

    /// O que falta no setup obrigatório (tasks sem repo, conventions no template,
    /// mapeamento ausente).
    pub fn setup_needs(&self) -> SetupNeeds {
        SetupNeeds {
            unlinked: self
                .tasks
                .iter()
                .filter(|t| t.prs.is_empty())
                .map(|t| t.id.clone())
                .collect(),
            leaky_branches: self
                .tasks
                .iter()
                .filter(|t| t.prs.iter().any(|pr| branch_leaks_id(&pr.branch)))
                .map(|t| t.id.clone())
                .collect(),
            conventions_template: is_template(&self.conventions),
            mapping_missing: !self.home().join("setup.md").exists(),
        }
    }

    pub fn setup_needed(&self) -> bool {
        self.setup_needs().any()
    }

    /// Abre o chat interativo de setup (claude pode escrever na área do jaum).
    pub fn setup_start(&mut self) {
        let result = Setup::new(
            &self.store,
            &self.executor,
            self.home(),
            self.repos.clone(),
            self.conventions.clone(),
        )
        .start();
        match result {
            Ok((session, sid)) => {
                let cwd = self.home();
                self.open_session(SessionKind::Setup, None, session, Vec::new(), sid, cwd);
                self.status_msg = "setup: chat aberto".into();
            }
            Err(e) => self.status_msg = format!("setup falhou: {e}"),
        }
    }

    // --- ações (efeitos colaterais) ---------------------------------------

    pub fn play_selected(&mut self) {
        let Some(id) = self.target_task_id() else {
            self.status_msg = "nenhuma task selecionada".into();
            return;
        };
        // já há uma sessão de play viva para esta task? foca nela em vez de abrir
        // uma duplicata (dois claudes na mesma worktree brigariam pelo working dir).
        if let Some(idx) = self.sessions.iter().position(|e| {
            e.is_live() && e.kind == SessionKind::Play && e.task.as_deref() == Some(id.as_str())
        }) {
            self.session_selected = idx;
            self.tab = Tab::Session;
            self.status_msg = format!("play de {id} já está aberto");
            return;
        }
        let result = Play::new(
            &self.store,
            &self.git,
            &self.executor,
            &self.work_dir,
            self.repos.clone(),
            self.conventions.clone(),
        )
        .start(&id);
        match result {
            Ok(ps) => {
                let PlaySession {
                    id,
                    session,
                    worktrees,
                    claude_session_id,
                    ..
                } = ps;
                let cwd = worktrees[0].1.clone();
                self.open_session(
                    SessionKind::Play,
                    Some(id.clone()),
                    session,
                    worktrees,
                    claude_session_id,
                    cwd,
                );
                self.status_msg = format!("play iniciado em {id}");
            }
            Err(e) => self.status_msg = format!("play falhou: {e}"),
        }
        let _ = self.refresh();
    }

    pub fn review_selected(&mut self) {
        let Some(id) = self.target_task_id() else {
            return;
        };
        self.review_report = self.load_review(&id);
        let review = Review::new(
            &self.store,
            &self.git,
            &self.gh,
            &self.executor,
            &self.docs_dir,
            self.repos.clone(),
            self.conventions.clone(),
        );
        let cwd = review.review_cwd(&id);
        match review.start(&id) {
            Ok((session, sid)) => {
                self.open_session(
                    SessionKind::Review,
                    Some(id.clone()),
                    session,
                    Vec::new(),
                    sid,
                    cwd,
                );
                self.status_msg = format!("review read-only de {id}");
            }
            Err(e) => self.status_msg = format!("review falhou: {e}"),
        }
    }

    /// Handoff: injeta os findings do review na sessão de play da task (abrindo
    /// uma se não houver), para o claude corrigir as pendências.
    pub fn handoff_selected(&mut self) {
        let Some(id) = self.target_task_id() else {
            self.status_msg = "nenhuma task selecionada".into();
            return;
        };
        let Some(report) = self.load_review(&id) else {
            self.status_msg = "rode o review (R) antes do handoff".into();
            return;
        };
        if report.is_clean() {
            self.status_msg = "review limpo — nada a corrigir".into();
            return;
        }

        // acha (ou abre) uma sessão de play viva para a task
        let find = |s: &[SessionEntry]| {
            s.iter().position(|e| {
                e.is_live()
                    && e.kind == SessionKind::Play
                    && e.task.as_deref() == Some(id.as_str())
            })
        };
        let mut idx = find(&self.sessions);
        if idx.is_none() {
            self.play_selected();
            idx = find(&self.sessions);
        }
        let Some(idx) = idx else {
            return; // play falhou (status já setado)
        };

        let msg = jaum_flows::review::handoff_message(&report);
        if let Some(e) = self.sessions.get_mut(idx) {
            if let Some(s) = &mut e.session {
                let _ = s.write_line(&msg);
            }
            self.session_selected = idx;
            self.tab = Tab::Session;
            self.status_msg = format!("findings de {id} enviados ao play");
        }
    }

    pub fn finish_selected(&mut self) {
        let Some(id) = self.target_task_id() else {
            return;
        };
        match Finish::new(&self.store, &self.gh, self.repos.clone()).run(&id) {
            Ok(state) => {
                self.status_msg = format!("finish {id}: merge {state:?} (merge é comando seu)")
            }
            Err(e) => self.status_msg = format!("finish falhou: {e}"),
        }
        let _ = self.refresh();
    }

    // --- jobs assíncronos (ingest/captura/init com logs ao vivo) ----------

    /// Há um job ainda rodando?
    pub fn job_running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| !j.finished)
    }

    /// Caminho do `.backlog/` do projeto atual (para construir um Store na thread).
    fn backlog_path(&self) -> PathBuf {
        self.config
            .projects
            .get(self.current)
            .map(|p| p.backlog.clone())
            .unwrap_or_default()
    }

    /// Inicia o ingest em background, com logs ao vivo no overlay.
    pub fn start_ingest_job(&mut self) {
        if self.job_running() {
            return;
        }
        let backlog = self.backlog_path();
        let docs_dir = self.docs_dir.clone();
        let add_dirs: Vec<PathBuf> = self.repos.values().cloned().collect();
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Ingest,
            title: "ingest".into(),
            logs: vec!["varrendo docs e repos com o claude…".into()],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let executor = ClaudeExecutor::new();
            let ingest = Ingest::new(&store, &executor, docs_dir, add_dirs);
            let mut on_line = |s: &str| {
                let _ = tx.send(JobMsg::Log(s.to_string()));
            };
            let done = match ingest.run_logged(&mut on_line) {
                Ok(o) => Ok(format!(
                    "ingest: {} stub(s), {} doc(s)",
                    o.created.len(),
                    o.docs_imported
                )),
                Err(e) => Err(format!("ingest falhou: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Roda a captura estruturada do review da task selecionada em background:
    /// um `claude -p` read-only que grava o `.review.md` (findings + veredictos).
    pub fn start_review_job(&mut self) {
        let Some(id) = self.target_task_id() else {
            self.status_msg = "nenhuma task selecionada".into();
            return;
        };
        if self.job_running() {
            return;
        }
        let backlog = self.backlog_path();
        let docs_dir = self.docs_dir.clone();
        let repos = self.repos.clone();
        let conventions = self.conventions.clone();
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Review,
            title: format!("review {id}"),
            logs: vec![format!("revisando {id} contra docs e constraints…")],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let git = Git::new();
            let gh = Gh::new();
            let executor = ClaudeExecutor::new();
            let review = Review::new(&store, &git, &gh, &executor, docs_dir, repos, conventions);
            let mut on_line = |s: &str| {
                let _ = tx.send(JobMsg::Log(s.to_string()));
            };
            let done = match review.capture_logged(&id, &mut on_line) {
                Ok(r) => Ok(format!(
                    "review {id}: {} finding(s), {}",
                    r.findings.len(),
                    if r.is_clean() { "LIMPO" } else { "SUJO" }
                )),
                Err(e) => Err(format!("review falhou: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Roda a análise de paralelismo (quais tasks colidem) em background, grava
    /// `parallel.json` e atualiza os badges do Board ao terminar.
    pub fn start_parallel_job(&mut self) {
        if self.job_running() {
            return;
        }
        let backlog = self.backlog_path();
        let root = self.docs_dir.clone();
        let repos = self.repos.clone();
        let out_file = self.parallel_file();
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Parallel,
            title: "análise de paralelismo".into(),
            logs: vec!["analisando quais tasks colidem…".into()],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let executor = ClaudeExecutor::new();
            let parallel = Parallel::new(&store, &executor, root, repos);
            let mut on_line = |s: &str| {
                let _ = tx.send(JobMsg::Log(s.to_string()));
            };
            let done = match parallel.analyze_logged(&mut on_line) {
                Ok(r) => {
                    if let Some(parent) = out_file.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Ok(json) = serde_json::to_string_pretty(&r) {
                        let _ = std::fs::write(&out_file, json);
                    }
                    Ok(format!("paralelismo: {} conflito(s)", r.conflicts.len()))
                }
                Err(e) => Err(format!("análise de paralelismo falhou: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Inicia a captura investigada (dica do usuário) em background.
    pub fn start_capture_job(&mut self, hint: &str) {
        let hint = hint.trim().to_string();
        if hint.is_empty() {
            self.status_msg = "captura cancelada (vazia)".into();
            return;
        }
        if self.job_running() {
            return;
        }
        let backlog = self.backlog_path();
        let docs_dir = self.docs_dir.clone();
        let add_dirs: Vec<PathBuf> = self.repos.values().cloned().collect();
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Capture,
            title: "captura (claude investiga)".into(),
            logs: vec![format!("investigando: {hint}")],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let store = Store::new(&backlog);
            let executor = ClaudeExecutor::new();
            let ingest = Ingest::new(&store, &executor, docs_dir, add_dirs);
            let mut on_line = |s: &str| {
                let _ = tx.send(JobMsg::Log(s.to_string()));
            };
            let done = match ingest.capture_logged(&hint, &mut on_line) {
                Ok(o) => {
                    let ids: Vec<String> = o.created.iter().map(|t| t.id.clone()).collect();
                    Ok(format!("claude criou: {}", ids.join(", ")))
                }
                Err(e) => Err(format!("captura falhou: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Inicia o `init` de um novo projeto em background (detecta repos e registra).
    pub fn start_init_job(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            self.status_msg = "init cancelado (vazio)".into();
            return;
        }
        if self.job_running() {
            return;
        }
        let root = expand_tilde(path);
        let (tx, rx) = channel();
        self.job = Some(Job {
            kind: JobKind::Init,
            title: format!("init {path}"),
            logs: vec![format!("detectando repos em {}", root.display())],
            rx,
            finished: false,
            scroll: 0,
            follow: true,
        });
        self.job_overlay = true;
        std::thread::spawn(move || {
            let done = match crate::config::init_project(&root, &[]) {
                Ok(p) => {
                    let _ = tx.send(JobMsg::Log(format!("projeto '{}' registrado", p.name)));
                    if p.repos.is_empty() {
                        let _ = tx.send(JobMsg::Log("nenhum repo detectado".into()));
                    }
                    for r in &p.repos {
                        let _ = tx.send(JobMsg::Log(format!("repo {} -> {}", r.slug, r.path.display())));
                    }
                    Ok(p.name)
                }
                Err(e) => Err(format!("init falhou: {e}")),
            };
            let _ = tx.send(JobMsg::Done(done));
        });
    }

    /// Drena as mensagens do job atual (chamado a cada frame pelo event loop).
    pub fn poll_job(&mut self) {
        let mut outcome: Option<(JobKind, Result<String, String>)> = None;
        if let Some(job) = self.job.as_mut() {
            while let Ok(msg) = job.rx.try_recv() {
                match msg {
                    JobMsg::Log(l) => job.logs.push(l),
                    JobMsg::Done(r) => {
                        job.finished = true;
                        outcome = Some((job.kind, r));
                    }
                }
            }
        }
        let Some((kind, r)) = outcome else { return };
        // ingest bem-sucedido encadeia a análise de paralelismo (atende "rodar no ingest").
        let auto_parallel = matches!(kind, JobKind::Ingest) && r.is_ok();
        match (kind, r) {
            (JobKind::Init, Ok(name)) => {
                if let Ok(cfg) = GlobalConfig::load() {
                    self.config = cfg;
                    if let Some(idx) = self.config.projects.iter().position(|p| p.name == name) {
                        self.load_project(idx);
                    }
                }
                if self.setup_needed() {
                    self.status_msg = format!("projeto '{name}' registrado — setup pendente (S)");
                } else {
                    self.status_msg = format!("projeto '{name}' registrado");
                }
                if let Some(j) = self.job.as_mut() {
                    j.logs.push(format!("— projeto '{name}' pronto"));
                }
            }
            (_, Ok(msg)) | (_, Err(msg)) => {
                self.status_msg = msg.clone();
                if let Some(j) = self.job.as_mut() {
                    j.logs.push(format!("— {msg}"));
                }
                let _ = self.refresh();
            }
        }
        if auto_parallel {
            self.start_parallel_job();
        }
    }

    /// Fecha o overlay de logs (o job, se ainda rodar, segue em background).
    pub fn dismiss_job(&mut self) {
        self.job_overlay = false;
        if self.job.as_ref().is_some_and(|j| j.finished) {
            self.job = None;
        }
    }

    /// Sobe nos logs do job (desliga o follow para travar a leitura).
    pub fn job_scroll_up(&mut self) {
        if let Some(j) = self.job.as_mut() {
            j.follow = false;
            j.scroll = j.scroll.saturating_sub(1);
        }
    }

    /// Desce nos logs do job. Ao chegar no fim, religa o follow (ao vivo).
    pub fn job_scroll_down(&mut self) {
        if let Some(j) = self.job.as_mut() {
            j.follow = false;
            j.scroll = j.scroll.saturating_add(1);
        }
    }

    /// Vai pro topo dos logs.
    pub fn job_scroll_top(&mut self) {
        if let Some(j) = self.job.as_mut() {
            j.follow = false;
            j.scroll = 0;
        }
    }

    /// Volta a acompanhar o fim ao vivo.
    pub fn job_follow(&mut self) {
        if let Some(j) = self.job.as_mut() {
            j.follow = true;
        }
    }

    /// Registra escopo extra como deferred e cria um backlog novo.
    pub fn defer(&mut self, text: &str) {
        let Some(id) = self.target_task_id() else {
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

    /// Relê o estado do disco para pegar edições externas (ex.: o que a sessão de
    /// setup gravou). Reage NA HORA a eventos do file watcher; com um fallback por
    /// tempo (~2s) caso o watcher não esteja disponível. Preserva a seleção.
    pub fn tick_reload(&mut self) {
        let fs_event = self
            .watch_rx
            .as_ref()
            .map(|rx| rx.try_iter().count() > 0)
            .unwrap_or(false);
        if fs_event || self.last_reload.elapsed() >= Duration::from_millis(2000) {
            self.last_reload = Instant::now();
            self.reload_conventions();
            let _ = self.refresh();
        }
    }

    // --- toast (snackbar temporário) --------------------------------------

    /// Detecta mudança em `status_msg` e (re)inicia o timer do toast. Chamado a
    /// cada frame pelo event loop.
    pub fn tick_toast(&mut self) {
        if self.status_msg != self.toast_shown {
            self.toast_shown = self.status_msg.clone();
            self.toast_started = Some(Instant::now());
        }
    }

    /// (Re)mostra o toast com a mensagem atual AGORA, mesmo que o texto não tenha
    /// mudado. Usado após uma ação para dar feedback no retry (ex.: re-tentar um
    /// comando que falha igual mostra o erro de novo em vez de silêncio).
    pub fn rearm_toast(&mut self) {
        self.toast_shown = self.status_msg.clone();
        self.toast_started = Some(Instant::now());
    }

    /// Texto do toast enquanto visível (alguns segundos), senão `None`.
    pub fn active_toast(&self) -> Option<&str> {
        self.toast_started
            .filter(|t| t.elapsed() < Duration::from_millis(3500))
            .map(|_| self.status_msg.as_str())
    }

    // --- captura rápida ----------------------------------------------------

    /// Inicia a captura de texto de um tipo (abre o input no rodapé).
    pub fn start_input(&mut self, kind: InputKind) {
        self.input = Some((kind, String::new()));
    }

    /// Abre o input de `init` já preenchido com o diretório atual.
    pub fn start_init_input(&mut self) {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.input = Some((InputKind::InitPath, cwd));
    }

    /// Despacha o texto capturado conforme o tipo.
    pub fn submit_input(&mut self, kind: InputKind, text: String) {
        match kind {
            InputKind::Defer => self.defer(&text),
            InputKind::Convention => self.add_convention(&text),
            InputKind::NewTask => self.new_task_quick(&text),
            InputKind::NewTaskClaude => self.start_capture_job(&text),
            InputKind::InitPath => self.start_init_job(&text),
        }
    }

    /// Anexa uma convenção ao `conventions.md` (vale nas próximas sessões).
    pub fn add_convention(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.status_msg = "convenção cancelada (vazia)".into();
            return;
        }
        let mut content = std::fs::read_to_string(&self.conventions_path).unwrap_or_default();
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!("- {text}\n"));
        match std::fs::write(&self.conventions_path, &content) {
            Ok(()) => {
                self.conventions = content;
                self.status_msg = "convenção adicionada".into();
            }
            Err(e) => self.status_msg = format!("falha ao gravar convenção: {e}"),
        }
    }

    /// Cria uma task rápida: o texto vira o objetivo (sem LLM).
    pub fn new_task_quick(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.status_msg = "task cancelada (vazia)".into();
            return;
        }
        let body = format!("## Objetivo\n\n{text}\n\n## Criterio de aceite\n- [ ] \n");
        match self
            .store
            .create_backlog(jaum_core::TaskType::Impl, Vec::new(), Vec::new(), body)
        {
            Ok(t) => self.status_msg = format!("task criada: {}", t.id),
            Err(e) => self.status_msg = format!("falha ao criar task: {e}"),
        }
        let _ = self.refresh();
    }

    // --- sessões (multi, em paralelo) -------------------------------------

    /// Abre uma sessão nova (vira a selecionada) e pula pra aba Session.
    pub(crate) fn open_session(
        &mut self,
        kind: SessionKind,
        task: Option<String>,
        session: Session,
        worktrees: Vec<(String, PathBuf)>,
        claude_session_id: String,
        cwd: PathBuf,
    ) {
        self.sessions.push(SessionEntry::spawn(
            kind,
            task,
            session,
            worktrees,
            claude_session_id,
            cwd,
        ));
        self.session_selected = self.sessions.len() - 1;
        self.sort_sessions(); // a recém-criada (atividade = agora) vai pro topo
        self.tab = Tab::Session;
        self.persist_sessions();
    }

    /// Caminho do arquivo de sessões persistidas (sobrevive ao shutdown).
    fn sessions_file(&self) -> PathBuf {
        self.work_dir.join("sessions.json")
    }

    /// Grava o snapshot das sessões em disco (best-effort, nunca derruba a TUI).
    pub(crate) fn persist_sessions(&self) {
        let records: Vec<SessionRecord> = self.sessions.iter().map(|e| e.to_record()).collect();
        if let Some(parent) = self.sessions_file().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&records) {
            let _ = std::fs::write(self.sessions_file(), json);
        }
    }

    /// Lê os registros persistidos (vazio se o arquivo não existe ou é inválido).
    fn load_session_records(&self) -> Vec<SessionRecord> {
        std::fs::read_to_string(self.sessions_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn selected_session(&self) -> Option<&SessionEntry> {
        self.sessions.get(self.session_selected)
    }

    pub fn session_next(&mut self) {
        if !self.sessions.is_empty() {
            self.session_selected = (self.session_selected + 1) % self.sessions.len();
        }
    }

    pub fn session_prev(&mut self) {
        if !self.sessions.is_empty() {
            let n = self.sessions.len();
            self.session_selected = (self.session_selected + n - 1) % n;
        }
    }

    /// Remove as worktrees de uma sessão de play (cleanup no encerramento). O
    /// branch fica no repo (a worktree é só a cópia de trabalho).
    fn cleanup_worktrees(&self, task: &Option<String>, worktrees: &[(String, PathBuf)]) {
        let Some(id) = task else { return };
        for (repo, _) in worktrees {
            if let Ok(task) = self.store.get(id)
                && let Some(link) = task.prs.iter().find(|p| &p.repo == repo)
                && let Some(repo_path) = self.repos.get(repo)
            {
                let _ = self.git.worktree_remove(repo_path, &link.branch);
            }
        }
    }

    /// Finaliza a sessão selecionada: encerra o processo e limpa as worktrees,
    /// mas a MANTÉM na lista como concluída (✓), histórico das sessões.
    pub fn finish_selected_session(&mut self) {
        let idx = self.session_selected;
        if idx >= self.sessions.len() {
            return;
        }
        if let Some(s) = &mut self.sessions[idx].session {
            let _ = s.kill();
        }
        self.sessions[idx].session = None;
        let task = self.sessions[idx].task.clone();
        let worktrees = self.sessions[idx].worktrees.clone();
        self.cleanup_worktrees(&task, &worktrees);
        self.sessions[idx].finished = true;
        self.status_msg = format!("sessão finalizada: {}", self.sessions[idx].name());
        self.persist_sessions();
    }

    /// Remove a sessão selecionada da lista (encerra se ainda rodando).
    pub fn close_selected_session(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let idx = self.session_selected.min(self.sessions.len() - 1);
        let mut e = self.sessions.remove(idx);
        if let Some(s) = &mut e.session {
            let _ = s.kill();
        }
        self.cleanup_worktrees(&e.task, &e.worktrees);
        if self.session_selected >= self.sessions.len() {
            self.session_selected = self.sessions.len().saturating_sub(1);
        }
        self.persist_sessions();
    }

    /// Encerra TODAS as sessões (troca de projeto / shutdown do daemon). PRESERVA
    /// as worktrees e o registro em disco: as vivas voltam resumidas no próximo
    /// boot. Remoção de worktree só acontece no `finish`/`close` explícito.
    pub fn stop_all_sessions(&mut self) {
        self.persist_sessions();
        for e in &mut self.sessions {
            if let Some(s) = &mut e.session {
                let _ = s.kill();
            }
        }
        self.sessions.clear();
        self.session_selected = 0;
    }

    /// Aplica os bytes pendentes de cada PTY no seu parser vt100.
    pub fn drain_pty(&mut self) {
        for e in &mut self.sessions {
            e.drain();
        }
        // o `drain` atualiza `last_activity`; reordena para manter o mais recente
        // no topo (sem perder a sessão selecionada).
        self.sort_sessions();
    }

    /// Ordena as sessões por atividade (mais recente no topo), preservando a
    /// seleção pela identidade (uuid) em vez do índice.
    pub fn sort_sessions(&mut self) {
        if self.sessions.len() < 2 {
            return;
        }
        let sel_key = self
            .sessions
            .get(self.session_selected)
            .map(|e| e.claude_session_id.clone());
        // mais recente primeiro; empate de atividade desempata pela criação (seq),
        // então a sessão mais nova fica no topo de forma determinística.
        self.sessions.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then(b.seq.cmp(&a.seq))
        });
        if let Some(key) = sel_key
            && let Some(i) = self
                .sessions
                .iter()
                .position(|e| e.claude_session_id == key)
        {
            self.session_selected = i;
        }
    }
}
