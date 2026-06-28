mod app;
mod config;
mod tui;

use std::path::PathBuf;

use anyhow::Result;

use crate::app::App;
use crate::config::{Config, init_project};

/// `jaum`            abre a TUI no projeto do diretório atual (ou no primeiro).
/// `jaum init [dirs] registra o projeto do cwd (auto-detecta repos, ou os dirs).
/// `jaum ingest`     varre o projeto com o claude e monta o backlog a partir dos docs.
/// `jaum list`       lista o backlog do projeto atual sem TUI.
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("init") => {
            let dirs: Vec<PathBuf> = args.map(PathBuf::from).collect();
            let cwd = std::env::current_dir()?;
            let project = init_project(&cwd, &dirs)?;
            println!(
                "projeto '{}' registrado em ~/jaum/config.toml",
                project.name
            );
            println!("  backlog: {}", project.backlog.display());
            if project.repos.is_empty() {
                println!("  repos: (nenhum detectado — adicione em ~/jaum/config.toml)");
            } else {
                for r in &project.repos {
                    println!("  repo: {} -> {}", r.slug, r.path.display());
                }
            }
            Ok(())
        }
        Some("ingest") => {
            let cfg = Config::load()?;
            let idx = select_project(&cfg)?;
            let project = &cfg.projects[idx];
            let root = project
                .backlog
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let store = jaum_core::Store::new(&project.backlog);
            let add_dirs: Vec<PathBuf> = project.repos.iter().map(|r| r.path.clone()).collect();
            let executor = jaum_adapters::ClaudeExecutor::new();
            let ingest = jaum_flows::ingest::Ingest::new(&store, &executor, root, add_dirs);

            eprintln!(
                "varrendo '{}' com o claude (pode levar alguns segundos)...",
                project.name
            );
            let created = ingest.run()?;
            println!("ingest: {} stub(s) criados", created.len());
            for t in &created {
                println!(
                    "  {:<10} {:?}  rfcs={:?} adrs={:?}",
                    t.id, t.task_type, t.rfcs, t.adrs
                );
            }
            Ok(())
        }
        Some("list") => {
            let cfg = Config::load()?;
            let idx = select_project(&cfg)?;
            let store = jaum_core::Store::new(&cfg.projects[idx].backlog);
            for t in store.list(None)? {
                println!("{:<10} {:<8?} {:?}", t.id, t.status, t.task_type);
            }
            Ok(())
        }
        _ => {
            let cfg = Config::load()?;
            let idx = select_project(&cfg)?;
            let app = App::new(cfg, idx)?;
            tui::run(app)
        }
    }
}

/// Escolhe o projeto: o do cwd, senão o primeiro. Erra se não há nenhum.
fn select_project(cfg: &Config) -> Result<usize> {
    config::ensure_usable(cfg)?;
    if let Ok(cwd) = std::env::current_dir()
        && let Some(p) = cfg.project_for_cwd(&cwd)
    {
        return Ok(cfg.projects.iter().position(|x| x.name == p.name).unwrap());
    }
    Ok(0)
}
