use jaum_core::Store;

/// Entrypoint provisório da fase 1: lista o backlog do `.backlog/` do diretório
/// atual. A TUI ratatui entra na fase 7.
fn main() -> anyhow::Result<()> {
    let store = Store::open();
    let tasks = store.list(None)?;
    if tasks.is_empty() {
        println!("(backlog vazio em {})", store.root().display());
        return Ok(());
    }
    for t in tasks {
        println!("{:<10} {:<8?} {:?}", t.id, t.status, t.task_type);
    }
    Ok(())
}
