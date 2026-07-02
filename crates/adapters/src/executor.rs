use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Guards de uma invocação, descritos de forma **tool-agnóstica**. Cada impl de
/// [`Executor`] traduz isto para as flags da sua ferramenta. Trocar de
/// ferramenta = nova impl que traduz `ExecFlags` diferente.
#[derive(Debug, Clone, Default)]
pub struct ExecFlags {
    /// Ferramentas a bloquear (ex.: merge). No Claude Code vira `--disallowedTools`.
    pub disallowed_tools: Vec<String>,
    /// Ferramentas explicitamente permitidas (review read-only restringe a isto).
    pub allowed_tools: Vec<String>,
    /// Texto extra de system prompt (as constraints vão aqui).
    pub append_system_prompt: Option<String>,
    /// Arquivo de settings que registra o PreToolUse hook (gerado pelo play).
    pub settings: Option<PathBuf>,
    /// Override de modelo.
    pub model: Option<String>,
    /// Diretório de trabalho (a worktree da task).
    pub cwd: Option<PathBuf>,
    /// UUID determinístico da sessão a CRIAR (`--session-id`). Permite resumir
    /// depois sem fazer parse do output.
    pub session_id: Option<String>,
    /// UUID da sessão a RETOMAR (`--resume`). Exclui `session_id` (resume vence).
    pub resume: Option<String>,
    /// Escape hatch para flags adicionais.
    pub extra: Vec<String>,
}

impl ExecFlags {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_disallowed<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.disallowed_tools
            .extend(tools.into_iter().map(Into::into));
        self
    }

    pub fn with_allowed<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_tools.extend(tools.into_iter().map(Into::into));
        self
    }

    pub fn with_append_system_prompt(mut self, text: impl Into<String>) -> Self {
        self.append_system_prompt = Some(text.into());
        self
    }

    /// Registra o settings json que carrega o PreToolUse hook (constraints `enforce: hook`).
    pub fn with_hook(mut self, settings: impl Into<PathBuf>) -> Self {
        self.settings = Some(settings.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Cria a sessão com um UUID conhecido (`--session-id`).
    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// Retoma uma sessão existente (`--resume`).
    pub fn with_resume(mut self, id: impl Into<String>) -> Self {
        self.resume = Some(id.into());
        self
    }

    /// Traduz para os argumentos do CLI `claude` (sem `--print` nem o prompt).
    /// Listas variádicas vêm antes das flags com valor; o prompt nunca vai
    /// depois de uma lista variádica (é sempre posicional no início) para não
    /// ser engolido por `--disallowedTools a b <prompt>`.
    fn to_claude_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if !self.disallowed_tools.is_empty() {
            a.push("--disallowedTools".to_string());
            a.extend(self.disallowed_tools.iter().cloned());
        }
        if !self.allowed_tools.is_empty() {
            a.push("--allowedTools".to_string());
            a.extend(self.allowed_tools.iter().cloned());
        }
        if let Some(s) = &self.append_system_prompt {
            a.push("--append-system-prompt".to_string());
            a.push(s.clone());
        }
        if let Some(p) = &self.settings {
            a.push("--settings".to_string());
            a.push(p.to_string_lossy().into_owned());
        }
        if let Some(m) = &self.model {
            a.push("--model".to_string());
            a.push(m.clone());
        }
        // resume e session-id são mutuamente exclusivos: retomar vence criar.
        if let Some(r) = &self.resume {
            a.push("--resume".to_string());
            a.push(r.clone());
        } else if let Some(s) = &self.session_id {
            a.push("--session-id".to_string());
            a.push(s.clone());
        }
        a.extend(self.extra.iter().cloned());
        a
    }
}

/// Executor plugável. O play/review usam `spawn_interactive`; o ingest usa
/// `spawn_oneshot`. Mantém o core agnóstico de qual ferramenta executa.
pub trait Executor {
    /// Roda uma vez de forma headless e devolve o stdout (ingest, `claude -p`).
    fn spawn_oneshot(&self, prompt: &str, flags: &ExecFlags) -> Result<String>;

    /// Abre uma sessão interativa num PTY (play/review iterativo).
    fn spawn_interactive(&self, prompt: &str, flags: &ExecFlags) -> Result<Session>;

    /// Como `spawn_oneshot`, mas chama `on_line` para cada linha de stdout
    /// enquanto o processo roda (logs ao vivo). Devolve o stdout completo. O
    /// default (para fakes) só delega ao `spawn_oneshot` sem streaming.
    fn spawn_oneshot_streaming(
        &self,
        prompt: &str,
        flags: &ExecFlags,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<String> {
        let out = self.spawn_oneshot(prompt, flags)?;
        for line in out.lines() {
            on_line(line);
        }
        Ok(out)
    }
}

/// Impl do [`Executor`] para o Claude Code (CLI `claude`).
pub struct ClaudeExecutor {
    bin: String,
}

impl Default for ClaudeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeExecutor {
    pub fn new() -> Self {
        Self {
            bin: "claude".to_string(),
        }
    }

    /// Aponta para um binário alternativo (usado em testes).
    pub fn with_bin(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

impl Executor for ClaudeExecutor {
    fn spawn_oneshot(&self, prompt: &str, flags: &ExecFlags) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        // prompt posicional primeiro, depois --print e as flags
        cmd.arg(prompt).arg("--print").args(flags.to_claude_args());
        if let Some(cwd) = &flags.cwd {
            cmd.current_dir(cwd);
        }
        let out = cmd
            .output()
            .with_context(|| format!("executando {} --print", self.bin))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            // com --output-format json o erro vai pro stdout como envelope;
            // extrai o campo `result` pra uma mensagem limpa.
            let detail = if !stderr.trim().is_empty() {
                stderr.trim().to_string()
            } else if let Some(msg) = serde_json::from_str::<serde_json::Value>(stdout.trim())
                .ok()
                .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(str::to_string))
            {
                msg
            } else {
                stdout.trim().to_string()
            };
            bail!(
                "{} --print falhou (exit {:?}): {detail}",
                self.bin,
                out.status.code()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn spawn_oneshot_streaming(
        &self,
        prompt: &str,
        flags: &ExecFlags,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(prompt).arg("--print").args(flags.to_claude_args());
        if let Some(cwd) = &flags.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("executando {} --print (stream)", self.bin))?;

        // stderr numa thread para não travar quando o buffer enche.
        let stderr = child.stderr.take();
        let err_handle = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_string(&mut s);
            }
            s
        });

        let stdout = child.stdout.take().context("sem stdout do claude")?;
        let reader = BufReader::new(stdout);
        let mut full = String::new();
        for line in reader.lines() {
            let line = line.context("lendo stdout do claude")?;
            on_line(&line);
            full.push_str(&line);
            full.push('\n');
        }

        let status = child.wait().context("aguardando o claude encerrar")?;
        let stderr = err_handle.join().unwrap_or_default();
        if !status.success() {
            // com stream-json o erro costuma vir no último evento `result`;
            // se stderr trouxe algo, usa; senão entrega o stdout cru.
            let detail = if !stderr.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                full.trim().to_string()
            };
            bail!(
                "{} --print (stream) falhou (exit {:?}): {detail}",
                self.bin,
                status.code()
            );
        }
        Ok(full)
    }

    fn spawn_interactive(&self, prompt: &str, flags: &ExecFlags) -> Result<Session> {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize::default()).context("abrindo PTY")?;

        let mut cmd = CommandBuilder::new(&self.bin);
        if !prompt.is_empty() {
            cmd.arg(prompt); // posicional primeiro
        }
        for arg in flags.to_claude_args() {
            cmd.arg(arg);
        }
        if let Some(cwd) = &flags.cwd {
            cmd.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("iniciando sessão {}", self.bin))?;
        let writer = pair.master.take_writer().context("obtendo writer do PTY")?;

        // Solta o slave: sem isso o reader nunca recebe EOF quando o filho sai.
        drop(pair.slave);

        Ok(Session {
            child,
            master: pair.master,
            writer: Some(writer),
        })
    }
}

/// Sessão interativa viva num PTY. O executor é dono do ciclo de vida; a TUI
/// (fase 7) lê de [`Session::reader`] e renderiza via vt100/tui-term.
pub struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Option<Box<dyn Write + Send>>,
}

impl Session {
    /// Escreve bytes crus no stdin da sessão.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        let w = self
            .writer
            .as_mut()
            .context("input da sessão já foi fechado")?;
        w.write_all(bytes).context("escrevendo no PTY")?;
        w.flush().context("flush no PTY")?;
        Ok(())
    }

    /// Escreve uma linha seguida de CR (Enter no PTY).
    pub fn write_line(&mut self, line: &str) -> Result<()> {
        self.write_input(line.as_bytes())?;
        self.write_input(b"\r")
    }

    /// Fecha o stdin (EOF para o filho).
    pub fn close_input(&mut self) {
        self.writer = None;
    }

    /// Clona um leitor do output do PTY (a TUI o consome numa thread).
    pub fn reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master
            .try_clone_reader()
            .context("clonando reader do PTY")
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                ..Default::default()
            })
            .context("redimensionando PTY")
    }

    /// `Some(success)` se já encerrou, `None` se ainda roda.
    pub fn try_wait(&mut self) -> Result<Option<bool>> {
        Ok(self.child.try_wait()?.map(|s| s.success()))
    }

    /// Bloqueia até encerrar; devolve se saiu com sucesso.
    pub fn wait(&mut self) -> Result<bool> {
        Ok(self.child.wait()?.success())
    }

    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().context("matando sessão")
    }
}
