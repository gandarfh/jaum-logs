use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gray_matter::Matter;
use gray_matter::engine::YAML;

use crate::error::JaumError;
use crate::model::{Constraint, Enforce, Repo, Status, Task, TaskType};

/// Dono do diretório `.backlog/`: a única fonte de verdade do backlog.
/// Toda leitura/escrita de task passa por aqui.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store apontando para `.backlog/` relativo ao diretório atual.
    pub fn open() -> Self {
        Self::new(".backlog")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn task_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.md"))
    }

    /// Caminho do relatório de review da task (`.backlog/TASK-NNN.review.md`).
    pub fn review_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.review.md"))
    }

    // --- leitura -----------------------------------------------------------

    /// Lê e parseia uma task de um caminho arbitrário.
    pub fn parse(&self, path: &Path) -> Result<Task> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("lendo task em {}", path.display()))?;

        let matter = Matter::<YAML>::new();
        let parsed = matter
            .parse::<Task>(&content)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("parseando frontmatter de {}", path.display()))?;

        let mut task = parsed.data.ok_or_else(|| JaumError::MalformedFrontmatter {
            path: path.display().to_string(),
        })?;
        task.body = parsed.content.trim().to_string();
        task.path = Some(path.to_path_buf());
        Ok(task)
    }

    /// Lista tasks, opcionalmente filtrando por status. Ordena por id.
    pub fn list(&self, status: Option<Status>) -> Result<Vec<Task>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("lendo diretório {}", self.root.display()))?
        {
            let path = entry?.path();
            if !is_task_file(&path) {
                continue;
            }
            let task = self.parse(&path)?;
            if status.is_none_or(|s| task.status == s) {
                out.push(task);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Busca a task por id (convenção: arquivo `TASK-NNN.md`).
    pub fn get(&self, id: &str) -> Result<Task> {
        let path = self.task_path(id);
        if !path.exists() {
            bail!(JaumError::TaskNotFound(id.to_string()));
        }
        self.parse(&path)
    }

    // --- escrita -----------------------------------------------------------

    /// Grava a task como markdown com frontmatter YAML. Usa `task.path` se
    /// presente, senão `.backlog/{id}.md`.
    pub fn write(&self, task: &Task) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("criando diretório {}", self.root.display()))?;

        let yaml = serde_yaml_ng::to_string(task).context("serializando frontmatter")?;
        // Normaliza independente de a crate prefixar `---` ou não.
        let yaml = yaml.trim_start_matches("---\n").trim_end();
        let body = task.body.trim();
        let content = format!("---\n{yaml}\n---\n\n{body}\n");

        let path = task
            .path
            .clone()
            .unwrap_or_else(|| self.task_path(&task.id));
        fs::write(&path, content)
            .with_context(|| format!("gravando task em {}", path.display()))?;
        Ok(())
    }

    /// Cria um stub de task `impl`/`backlog` a partir de referências de RFC.
    pub fn create_stub(&self, rfc_refs: &[String]) -> Result<Task> {
        let id = self.next_id()?;
        let task = Task {
            id: id.clone(),
            task_type: TaskType::Impl,
            status: Status::Backlog,
            rfcs: rfc_refs.to_vec(),
            adrs: Vec::new(),
            prs: Vec::new(),
            deferred: Vec::new(),
            constraints: Vec::new(),
            locks: Vec::new(),
            body: "## Objetivo\n\n## Criterio de aceite\n".to_string(),
            path: Some(self.task_path(&id)),
        };
        self.write(&task)?;
        Ok(task)
    }

    pub fn set_status(&self, id: &str, status: Status) -> Result<Task> {
        let mut task = self.get(id)?;
        task.status = status;
        self.write(&task)?;
        Ok(task)
    }

    /// Registra o número de um PR já existente (lido do `gh`) no vínculo do repo.
    pub fn set_pr(&self, id: &str, repo: &str, pr_num: u64) -> Result<Task> {
        let mut task = self.get(id)?;
        let link = task
            .prs
            .iter_mut()
            .find(|p| p.repo == repo)
            .ok_or_else(|| JaumError::PrLinkNotFound {
                id: id.to_string(),
                repo: repo.to_string(),
            })?;
        link.pr = pr_num;
        self.write(&task)?;
        Ok(task)
    }

    /// Registra escopo extra em `deferred` e materializa um novo backlog a
    /// partir dele — a borda contra o "projeto infinito". Devolve a task criada.
    pub fn add_deferred(&self, id: &str, text: &str) -> Result<Task> {
        let mut origin = self.get(id)?;
        origin.deferred.push(text.to_string());
        self.write(&origin)?;

        let new_id = self.next_id()?;
        let spawned = Task {
            id: new_id.clone(),
            task_type: TaskType::Impl,
            status: Status::Backlog,
            rfcs: Vec::new(),
            adrs: Vec::new(),
            prs: Vec::new(),
            deferred: Vec::new(),
            constraints: Vec::new(),
            locks: Vec::new(),
            body: format!(
                "## Objetivo\n\n{text}\n\n_Derivado de {id}._\n\n## Criterio de aceite\n"
            ),
            path: Some(self.task_path(&new_id)),
        };
        self.write(&spawned)?;
        Ok(spawned)
    }

    // --- consultas derivadas ----------------------------------------------

    pub fn linked_repos(&self, id: &str) -> Result<Vec<Repo>> {
        Ok(self.get(id)?.linked_repos())
    }

    pub fn constraints(&self, id: &str, kind: Enforce) -> Result<Vec<Constraint>> {
        Ok(self
            .get(id)?
            .constraints
            .into_iter()
            .filter(|c| c.enforce == kind)
            .collect())
    }

    // --- internos ----------------------------------------------------------

    /// Próximo id sequencial `TASK-NNN` (maior número existente + 1).
    fn next_id(&self) -> Result<String> {
        let mut max = 0u32;
        if self.root.exists() {
            for entry in fs::read_dir(&self.root)? {
                let path = entry?.path();
                if let Some(n) = parse_task_number(&path) {
                    max = max.max(n);
                }
            }
        }
        Ok(format!("TASK-{:03}", max + 1))
    }
}

/// Arquivo de task: `TASK-*.md`, excluindo os relatórios `*.review.md`.
fn is_task_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with("TASK-") && name.ends_with(".md") && !name.ends_with(".review.md")
}

/// Extrai o número de `TASK-NNN...` do nome do arquivo (review files incluídos).
fn parse_task_number(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("TASK-")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
