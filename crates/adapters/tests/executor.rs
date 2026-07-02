use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use jaum_adapters::{ClaudeExecutor, ExecFlags, Executor};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        p.push(format!("jaum-exec-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Fake `claude` que imprime cada argv em uma linha — deixa o teste assertar
/// exatamente como as flags foram montadas, sem rede.
fn fake_claude(dir: &TmpDir) -> String {
    let path = dir.0.join("claude");
    fs::write(&path, "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\"\n").unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn oneshot_monta_print_prompt_e_flags() {
    let dir = TmpDir::new("oneshot");
    let exec = ClaudeExecutor::with_bin(fake_claude(&dir));
    let flags = ExecFlags::new()
        .with_disallowed(["Edit", "Bash(git merge)"])
        .with_append_system_prompt("nao mergeie")
        .with_model("opus");

    let out = exec.spawn_oneshot("faça a task", &flags).unwrap();

    // prompt posicional vem antes de --print
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "faça a task");
    assert_eq!(lines[1], "--print");
    assert!(out.contains("--disallowedTools"));
    assert!(out.contains("Bash(git merge)")); // arg único, parênteses preservados
    assert!(out.contains("--append-system-prompt"));
    assert!(out.contains("nao mergeie"));
    assert!(out.contains("--model"));
    assert!(out.contains("opus"));
}

#[test]
fn oneshot_propaga_erro_de_exit_nao_zero() {
    let dir = TmpDir::new("oneshot-err");
    let path = dir.0.join("claude");
    fs::write(&path, "#!/usr/bin/env bash\necho 'boom' >&2\nexit 3\n").unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();

    let exec = ClaudeExecutor::with_bin(path.to_string_lossy().into_owned());
    let err = exec.spawn_oneshot("x", &ExecFlags::new()).unwrap_err();
    assert!(err.to_string().contains("boom"));
}

#[test]
fn streaming_repassa_cada_linha_e_devolve_tudo() {
    let dir = TmpDir::new("stream");
    let path = dir.0.join("claude");
    // imprime 3 linhas de "evento" no stdout
    fs::write(
        &path,
        "#!/usr/bin/env bash\nprintf 'um\\ndois\\ntres\\n'\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();

    let exec = ClaudeExecutor::with_bin(path.to_string_lossy().into_owned());
    let mut seen: Vec<String> = Vec::new();
    let mut on_line = |l: &str| seen.push(l.to_string());
    let out = exec
        .spawn_oneshot_streaming("x", &ExecFlags::new(), &mut on_line)
        .unwrap();

    assert_eq!(seen, vec!["um", "dois", "tres"]);
    assert_eq!(out, "um\ndois\ntres\n");
}

#[test]
fn streaming_propaga_erro_de_exit_nao_zero() {
    let dir = TmpDir::new("stream-err");
    let path = dir.0.join("claude");
    fs::write(&path, "#!/usr/bin/env bash\necho 'kaboom' >&2\nexit 4\n").unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();

    let exec = ClaudeExecutor::with_bin(path.to_string_lossy().into_owned());
    let mut on_line = |_: &str| {};
    let err = exec
        .spawn_oneshot_streaming("x", &ExecFlags::new(), &mut on_line)
        .unwrap_err();
    assert!(err.to_string().contains("kaboom"));
}

#[test]
fn interactive_roundtrip_via_pty() {
    // usa `cat` como sessão: ecoa o que recebe.
    let exec = ClaudeExecutor::with_bin("cat");
    let mut session = exec.spawn_interactive("", &ExecFlags::new()).unwrap();

    let mut reader = session.reader().unwrap();
    session.write_line("ping").unwrap();
    session.write_input(&[0x04]).unwrap(); // Ctrl-D: EOF -> cat encerra

    let mut buf = String::new();
    reader.read_to_string(&mut buf).unwrap(); // bloqueia até EOF
    assert!(buf.contains("ping"), "PTY não ecoou o input:\n{buf:?}");
    assert!(session.wait().unwrap(), "cat deveria sair com sucesso");
}

#[test]
fn interactive_kill_encerra_sessao() {
    // `sleep 5` via extra args; matamos antes de terminar.
    let exec = ClaudeExecutor::with_bin("sleep");
    let flags = ExecFlags {
        extra: vec!["5".to_string()],
        ..Default::default()
    };
    let mut session = exec.spawn_interactive("", &flags).unwrap();

    assert!(
        session.try_wait().unwrap().is_none(),
        "deveria estar rodando"
    );
    session.kill().unwrap();
    // após kill, wait retorna (sem sucesso por ter sido morto)
    let success = session.wait().unwrap();
    assert!(!success, "sessão morta não deveria reportar sucesso");
}
