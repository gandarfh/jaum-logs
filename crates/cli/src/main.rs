mod app;
mod backend;
mod client;
mod config;
mod daemon;
mod protocol;
mod tui;

use std::path::PathBuf;

use anyhow::Result;

use crate::app::App;
use crate::config::{Config, init_project};

/// `jaum`            conecta no daemon (subindo-o se preciso) e abre a TUI cliente.
/// `jaum --daemon N` roda o daemon do projeto N (uso interno; auto-spawned).
/// `jaum shutdown`   derruba o daemon (encerra sessões).
/// `jaum --local`    abre a TUI antiga in-process (sem daemon; debug).
/// `jaum init [dirs] registra o projeto do cwd (auto-detecta repos, ou os dirs).
/// `jaum ingest`     varre o projeto com o claude e monta o backlog a partir dos docs.
/// `jaum list`       lista o backlog do projeto atual sem TUI.
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // daemon foreground (já destacado pelo spawner): roda o servidor.
        Some("--daemon") => {
            let idx: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let cfg = Config::load()?;
            let app = App::new(cfg, idx)?;
            let sock = daemon::socket_path()?;
            daemon::serve(&sock, app, 80, 24)
        }
        Some("shutdown") => {
            let sock = daemon::socket_path()?;
            if daemon::shutdown(&sock)? {
                println!("daemon encerrado");
            } else {
                println!("nenhum daemon rodando");
            }
            Ok(())
        }
        // derruba o daemon atual e reanexa (pega o binário recém-instalado).
        Some("restart") => {
            let sock = daemon::socket_path()?;
            let _ = daemon::shutdown(&sock)?;
            for _ in 0..200 {
                if !daemon::is_running(&sock) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            attach()
        }
        Some("--local") => {
            let cfg = Config::load()?;
            let idx = select_project(&cfg)?;
            let app = App::new(cfg, idx)?;
            tui::run(app)
        }
        Some("init") => {
            let dirs: Vec<PathBuf> = args.map(PathBuf::from).collect();
            let cwd = std::env::current_dir()?;
            let project = init_project(&cwd, &dirs)?;
            println!(
                "projeto '{}' registrado em ~/jaum/config.toml",
                project.name
            );
            println!("  docs:    {}", project.docs.display());
            println!("  backlog: {}", project.backlog.display());
            println!("  (o repo {} fica intocado)", project.root.display());
            if project.repos.is_empty() {
                println!("  repos: (nenhum detectado — adicione em ~/jaum/config.toml)");
            } else {
                for r in &project.repos {
                    println!("  repo: {} -> {}", r.slug, r.path.display());
                }
            }
            println!("\npróximo: abra o `jaum`, rode o ingest (i) e o setup (S).");
            Ok(())
        }
        Some("ingest") => {
            let cfg = Config::load()?;
            let idx = select_project(&cfg)?;
            let project = &cfg.projects[idx];
            let store = jaum_core::Store::new(&project.backlog);
            // varre os docs externos (~/jaum/<projeto>/docs) + os repos
            let add_dirs: Vec<PathBuf> = project.repos.iter().map(|r| r.path.clone()).collect();
            let executor = jaum_adapters::ClaudeExecutor::new();
            let ingest =
                jaum_flows::ingest::Ingest::new(&store, &executor, project.docs.clone(), add_dirs);

            eprintln!(
                "varrendo '{}' com o claude (pode levar alguns segundos)...",
                project.name
            );
            let outcome = ingest.run()?;
            println!(
                "ingest: {} stub(s) criados, {} doc(s) espelhados em docs/",
                outcome.created.len(),
                outcome.docs_imported
            );
            for t in &outcome.created {
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
        // default: anexa no daemon (subindo-o se preciso).
        _ => attach(),
    }
}

/// Conecta no daemon; se não houver um vivo, sobe-o destacado e espera o socket.
fn attach() -> Result<()> {
    let cfg = Config::load()?;
    let idx = select_project(&cfg)?;
    let sock = daemon::socket_path()?;

    if !daemon::is_running(&sock) {
        daemon::spawn_detached(idx)?;
        // espera o socket subir (até ~2s)
        for _ in 0..200 {
            if daemon::is_running(&sock) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !daemon::is_running(&sock) {
            anyhow::bail!("o daemon não subiu; veja ~/jaum/daemon.log");
        }
    }
    client::run(&sock)
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
