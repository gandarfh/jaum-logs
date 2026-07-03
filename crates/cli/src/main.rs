mod app;
mod client;
mod config;
mod daemon;
mod keymap;
mod protocol;
mod snapshot;
mod tui;

use std::path::PathBuf;

use anyhow::Result;

use crate::app::App;
use crate::config::{Config, init_project};

/// `jaum`            connects to the daemon (starting it if needed) and opens the client TUI.
/// `jaum --daemon N` runs the daemon for project N (internal use; auto-spawned).
/// `jaum shutdown`   shuts the daemon down (stops sessions).
/// `jaum --local`    opens the old in-process TUI (no daemon; debug).
/// `jaum init [dirs] registers the cwd project (auto-detects repos, or the given dirs).
/// `jaum ingest`     scans the project with claude and builds the backlog from the docs.
/// `jaum list`       lists the current project's backlog without the TUI.
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // foreground daemon (already detached by the spawner): runs the server.
        Some("--daemon") => {
            let idx: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let cfg = Config::load()?;
            let app = App::new(cfg, idx)?;
            let sock = daemon::socket_path()?;
            daemon::serve(&sock, app)
        }
        Some("shutdown") => {
            let sock = daemon::socket_path()?;
            if daemon::shutdown(&sock)? {
                println!("daemon stopped");
            } else {
                println!("no daemon running");
            }
            Ok(())
        }
        // shut down the current daemon and reattach (picks up the freshly installed binary).
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
                "project '{}' registered in ~/jaum/config.toml",
                project.name
            );
            println!("  docs:    {}", project.docs.display());
            println!("  backlog: {}", project.backlog.display());
            println!("  (the repo {} stays untouched)", project.root.display());
            if project.repos.is_empty() {
                println!("  repos: (none detected — add them in ~/jaum/config.toml)");
            } else {
                for r in &project.repos {
                    println!("  repo: {} -> {}", r.slug, r.path.display());
                }
            }
            println!("\nnext: open `jaum`, run ingest (i) and setup (S).");
            Ok(())
        }
        Some("ingest") => {
            let cfg = Config::load()?;
            let idx = select_project(&cfg)?;
            let project = &cfg.projects[idx];
            let store = jaum_core::Store::new(&project.backlog);
            // scan the external docs (~/jaum/<project>/docs) + the repos
            let add_dirs: Vec<PathBuf> = project.repos.iter().map(|r| r.path.clone()).collect();
            let executor = jaum_adapters::ClaudeExecutor::new();
            let ingest =
                jaum_flows::ingest::Ingest::new(&store, &executor, project.docs.clone(), add_dirs);

            eprintln!(
                "scanning '{}' with claude (may take a few seconds)...",
                project.name
            );
            let outcome = ingest.run()?;
            println!(
                "ingest: {} stub(s) created, {} doc(s) mirrored into docs/",
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
        // default: attach to the daemon (starting it if needed).
        _ => attach(),
    }
}

/// Connects to the daemon; if none is alive, starts it detached and waits for the socket.
fn attach() -> Result<()> {
    let cfg = Config::load()?;
    let idx = select_project(&cfg)?;
    let sock = daemon::socket_path()?;

    if !daemon::is_running(&sock) {
        daemon::spawn_detached(idx)?;
        // wait for the socket to come up (up to ~2s)
        for _ in 0..200 {
            if daemon::is_running(&sock) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !daemon::is_running(&sock) {
            anyhow::bail!("the daemon did not come up; see ~/jaum/daemon.log");
        }
    }
    client::run(&sock)
}

/// Picks the project: the cwd's, otherwise the first. Errors if there is none.
fn select_project(cfg: &Config) -> Result<usize> {
    config::ensure_usable(cfg)?;
    if let Ok(cwd) = std::env::current_dir()
        && let Some(p) = cfg.project_for_cwd(&cwd)
    {
        return Ok(cfg.projects.iter().position(|x| x.name == p.name).unwrap());
    }
    Ok(0)
}
