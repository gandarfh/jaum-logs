//! Daemon: sole owner of the state (`App`) and the PTYs. Renders the whole `App`
//! into an in-memory `Buffer` (TestBackend) and exposes cell diffs to clients. The
//! client is pure render — all logic (render/handle_key/PTY) is reused from here.

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::event::KeyEvent;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::App;
use crate::backend::{diff_cells, full_cells};
use crate::protocol::{ClientMsg, ServerMsg, WireCell, read_msg, write_msg};
use crate::tui;

/// Daemon socket path (`~/jaum/daemon.sock`).
pub fn socket_path() -> Result<PathBuf> {
    Ok(crate::config::jaum_home()?.join("daemon.sock"))
}

/// Is there a live daemon listening on this socket?
pub fn is_running(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}

/// Starts the daemon detached from the terminal (`setsid`), with stdio in
/// `~/jaum/daemon.log`. Survives closing the terminal tab (immune to SIGHUP).
pub fn spawn_detached(idx: usize) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log_path = crate::config::jaum_home()?.join("daemon.log");
    spawn_daemon_process(&exe, idx, &log_path)
}

/// Spawns `exe --daemon <idx>` in its own session with stdio in `log_path`.
pub(crate) fn spawn_daemon_process(exe: &Path, idx: usize, log_path: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    if let Some(p) = log_path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let err = out.try_clone()?;

    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .arg(idx.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // SAFETY: setsid() is async-signal-safe; we only detach the child from the terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

/// Asks the daemon to shut down. `Ok(false)` if no daemon was running.
pub fn shutdown(sock: &Path) -> Result<bool> {
    if !is_running(sock) {
        return Ok(false);
    }
    let mut s = UnixStream::connect(sock)?;
    write_msg(&mut s, &ClientMsg::Shutdown)?;
    Ok(true)
}

/// Internal server event (connections and client messages).
enum Event {
    Connect(u64, UnixStream),
    Client(u64, ClientMsg),
    Disconnect(u64),
}

/// Starts the server: accepts clients on `sock`, renders the `App` and broadcasts
/// diffs. Blocks until it receives `Shutdown`. The daemon runs on the calling thread
/// (the `App`/PTY never crosses threads); only sockets go to helper threads.
pub fn serve(sock: &Path, app: App, cols: u16, rows: u16) -> Result<()> {
    if is_running(sock) {
        bail!("daemon already running at {}", sock.display());
    }
    let _ = std::fs::remove_file(sock); // orphan socket
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(sock)?;

    let (tx, rx) = channel::<Event>();
    spawn_acceptor(listener, tx);

    let mut daemon = Daemon::new(app, cols, rows)?;
    let mut clients: HashMap<u64, UnixStream> = HashMap::new();
    let mut pending_full: HashSet<u64> = HashSet::new();
    let mut running = true;

    while running {
        // 1) drain events (short wait for ~60fps when idle)
        match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(ev) => {
                handle_event(
                    ev,
                    &mut daemon,
                    &mut clients,
                    &mut pending_full,
                    &mut running,
                );
                while let Ok(ev) = rx.try_recv() {
                    handle_event(
                        ev,
                        &mut daemon,
                        &mut clients,
                        &mut pending_full,
                        &mut running,
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // 2) render + broadcast
        daemon.tick();
        let diff = daemon.render_diff();
        let full = (!pending_full.is_empty()).then(|| daemon.full_frame());

        let mut dead = Vec::new();
        for (id, w) in clients.iter_mut() {
            let res = if pending_full.contains(id) {
                let (cols, rows, cells) = full.clone().unwrap();
                write_msg(w, &ServerMsg::FrameFull { cols, rows, cells })
            } else if !diff.is_empty() {
                write_msg(w, &ServerMsg::FrameDiff(diff.clone()))
            } else {
                Ok(())
            };
            if res.is_err() {
                dead.push(*id);
            }
        }
        for id in dead {
            clients.remove(&id);
        }
        pending_full.clear();
    }

    // shutdown: stop the sessions (clean up worktrees) and remove the socket.
    daemon.app_mut().stop_all_sessions();
    let _ = std::fs::remove_file(sock);
    Ok(())
}

/// Thread that accepts connections and, per client, a `ClientMsg` reader thread.
fn spawn_acceptor(listener: UnixListener, tx: Sender<Event>) {
    thread::spawn(move || {
        let mut next_id = 0u64;
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let id = next_id;
            next_id += 1;
            let Ok(reader) = stream.try_clone() else {
                continue;
            };
            if tx.send(Event::Connect(id, stream)).is_err() {
                break;
            }
            let txr = tx.clone();
            thread::spawn(move || {
                let mut r = BufReader::new(reader);
                loop {
                    match read_msg::<_, ClientMsg>(&mut r) {
                        Ok(Some(m)) => {
                            if txr.send(Event::Client(id, m)).is_err() {
                                break;
                            }
                        }
                        _ => {
                            let _ = txr.send(Event::Disconnect(id));
                            break;
                        }
                    }
                }
            });
        }
    });
}

/// Processes an event by mutating the loop state.
fn handle_event(
    ev: Event,
    daemon: &mut Daemon,
    clients: &mut HashMap<u64, UnixStream>,
    pending_full: &mut HashSet<u64>,
    running: &mut bool,
) {
    match ev {
        Event::Connect(id, stream) => {
            clients.insert(id, stream);
            pending_full.insert(id); // new client gets a full frame
        }
        Event::Disconnect(id) => {
            clients.remove(&id);
            pending_full.remove(&id);
        }
        Event::Client(id, ClientMsg::Key(k)) => {
            let detach = daemon.feed_key(k);
            if let Some(path) = daemon.take_editor_request()
                && let Some(w) = clients.get_mut(&id)
            {
                let _ = write_msg(w, &ServerMsg::RunEditor { path });
            }
            if detach {
                if let Some(w) = clients.get_mut(&id) {
                    let _ = write_msg(w, &ServerMsg::Detach);
                }
                clients.remove(&id);
                pending_full.remove(&id);
            }
        }
        Event::Client(_, ClientMsg::Mouse(ev)) => daemon.feed_mouse(ev),
        Event::Client(_, ClientMsg::Resize { cols, rows }) => {
            daemon.resize(cols, rows);
            // everyone reapplies: the size changed (last-writer-wins)
            pending_full.extend(clients.keys().copied());
        }
        Event::Client(_, ClientMsg::EditorDone) => daemon.editor_done(),
        Event::Client(_, ClientMsg::Shutdown) => *running = false,
    }
}

/// Daemon render state: the `App` + an off-screen terminal.
pub struct Daemon {
    app: App,
    term: Terminal<TestBackend>,
    last: Buffer,
}

impl Daemon {
    pub fn new(app: App, cols: u16, rows: u16) -> Result<Self> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let term = Terminal::new(TestBackend::new(cols, rows))?;
        let last = Buffer::empty(Rect::new(0, 0, cols, rows));
        Ok(Self { app, term, last })
    }

    /// Drains PTY/jobs/toast and syncs the PTY size with the session pane.
    pub fn tick(&mut self) {
        self.app.drain_pty();
        self.app.poll_job();
        self.app.tick_reload();
        self.app.tick_pr_sync();
        self.app.tick_toast();
        let (w, h) = {
            let b = self.term.backend().buffer();
            (b.area.width, b.area.height)
        };
        tui::sync_pty_to(&mut self.app, w, h);
    }

    /// Re-renders and returns only the cells that changed since the last render.
    pub fn render_diff(&mut self) -> Vec<WireCell> {
        let app = &self.app;
        let _ = self.term.draw(|f| tui::render(f, app));
        let cur = self.term.backend().buffer().clone();
        let diff = diff_cells(&self.last, &cur);
        self.last = cur;
        diff
    }

    /// Full frame of the current state (for a new client's attach).
    pub fn full_frame(&self) -> (u16, u16, Vec<WireCell>) {
        let b = self.term.backend().buffer();
        (b.area.width, b.area.height, full_cells(b))
    }

    /// Applies a key. Returns `true` when the user asked to quit: in the daemon
    /// this means **detaching** the client — the daemon and sessions keep running.
    pub fn feed_key(&mut self, key: KeyEvent) -> bool {
        tui::handle_key(&mut self.app, key);
        if self.app.should_quit {
            self.app.should_quit = false;
            return true;
        }
        false
    }

    /// Forwards a mouse event (scroll/click) over the Session tab.
    pub fn feed_mouse(&mut self, ev: crossterm::event::MouseEvent) {
        let (w, h) = {
            let b = self.term.backend().buffer();
            (b.area.width, b.area.height)
        };
        tui::handle_mouse(&mut self.app, ev, w, h);
    }

    /// Resizes the off-screen backend (and the PTY on the next tick). Forces a full
    /// frame on the next render (new area).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        self.term.backend_mut().resize(cols, rows);
        tui::sync_pty_to(&mut self.app, cols, rows);
        self.last = Buffer::empty(Rect::new(0, 0, cols, rows));
    }

    /// If the user asked to edit `conventions.md` (`e`), returns the path for the
    /// client to run `$EDITOR` and clears the request.
    pub fn take_editor_request(&mut self) -> Option<String> {
        if self.app.edit_request {
            self.app.edit_request = false;
            Some(self.app.conventions_path.to_string_lossy().into_owned())
        } else {
            None
        }
    }

    /// Reloads the conventions after the client finishes editing.
    pub fn editor_done(&mut self) {
        self.app.reload_conventions();
        self.app.status_msg = "conventions.md updated".into();
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Project};
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn app() -> App {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "jaum-daemon-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let backlog = dir.join(".backlog");
        fs::create_dir_all(&backlog).unwrap();
        fs::write(
            backlog.join("TASK-001.md"),
            "---\nid: TASK-001\ntype: impl\nstatus: wip\n---\n\n## Objective\nx\n",
        )
        .unwrap();
        let project = Project {
            name: "t".into(),
            root: dir.clone(),
            backlog,
            docs: dir.join("docs"),
            work_dir: dir.join(".jaum"),
            repos: Vec::new(),
        };
        App::new(
            Config {
                projects: vec![project],
            },
            0,
        )
        .unwrap()
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn render_full_and_incremental_diff() {
        let mut d = Daemon::new(app(), 80, 24).unwrap();
        let (w, h, cells) = d.full_frame();
        assert_eq!((w, h), (80, 24));
        assert_eq!(cells.len(), 80 * 24);

        // first render: differs from the blank buffer
        assert!(!d.render_diff().is_empty());
        // no state change: empty diff
        assert!(d.render_diff().is_empty());
        // switching tabs changes the header -> non-empty diff
        assert!(!d.feed_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert!(!d.render_diff().is_empty());
    }

    #[test]
    fn quit_becomes_detach_without_shutting_down() {
        let mut d = Daemon::new(app(), 80, 24).unwrap();
        assert!(d.feed_key(key('q')), "q should signal detach");
        assert!(!d.app_mut().should_quit, "should_quit should be reset");
    }

    #[test]
    fn resize_forces_full_on_next_render() {
        let mut d = Daemon::new(app(), 80, 24).unwrap();
        let _ = d.render_diff();
        assert!(d.render_diff().is_empty());
        d.resize(100, 30);
        let (w, h, _) = d.full_frame();
        assert_eq!((w, h), (100, 30));
        // new area -> next render returns everything
        assert!(!d.render_diff().is_empty());
    }

    #[test]
    fn spawn_daemon_process_creates_log_and_detaches() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "jaum-spawn-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let log = dir.join("nested").join("daemon.log");
        spawn_daemon_process(Path::new("/usr/bin/true"), 3, &log).unwrap();
        assert!(log.exists(), "log file should be created");
        let _ = fs::remove_dir_all(&dir);

        // an empty log path has no parent and cannot be opened
        assert!(spawn_daemon_process(Path::new("/usr/bin/true"), 3, Path::new("")).is_err());
    }

    #[test]
    fn shutdown_without_daemon_reports_false() {
        let mut sock = std::env::temp_dir();
        sock.push(format!(
            "jaum-noserve-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(!is_running(&sock));
        assert!(!shutdown(&sock).unwrap());
    }

    #[test]
    fn editor_request_roundtrip_and_mouse_tick() {
        let mut d = Daemon::new(app(), 80, 24).unwrap();
        assert_eq!(d.take_editor_request(), None);

        assert!(!d.feed_key(key('e')), "edit request is not a detach");
        let path = d.take_editor_request().expect("editor requested");
        assert!(path.ends_with("conventions.md"));
        assert_eq!(d.take_editor_request(), None, "request must be cleared");

        d.editor_done();
        assert_eq!(d.app_mut().status_msg, "conventions.md updated");

        // scroll + tick only need to not disturb the render state
        d.feed_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollDown,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        d.tick();
    }

    #[test]
    fn serve_refuses_second_daemon_and_stops_on_shutdown() {
        let mut sock = std::env::temp_dir();
        sock.push(format!(
            "jaum-dup-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        // orphan socket file from a dead daemon: serve must clean it up
        fs::write(&sock, "stale").unwrap();

        let sock_c = sock.clone();
        let server = std::thread::spawn(move || serve(&sock_c, app(), 80, 24));
        for _ in 0..200 {
            if is_running(&sock) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(is_running(&sock));

        let err = serve(&sock, app(), 80, 24).unwrap_err();
        assert!(err.to_string().contains("already running"));

        assert!(shutdown(&sock).unwrap());
        server.join().unwrap().unwrap();
        assert!(!sock.exists());
    }

    #[test]
    fn serve_routes_editor_resize_mouse_and_detach() {
        use crate::protocol::{ClientMsg, ServerMsg, read_msg, write_msg};
        use crossterm::event::{MouseEvent, MouseEventKind};
        use std::os::unix::net::UnixStream;

        let mut sock = std::env::temp_dir();
        sock.push(format!(
            "jaum-route-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let sock_c = sock.clone();

        let client = std::thread::spawn(move || {
            let connect = || loop {
                if let Ok(c) = UnixStream::connect(&sock_c) {
                    break c;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let mut conn = connect();
            write_msg(&mut conn, &ClientMsg::Resize { cols: 80, rows: 24 }).unwrap();

            // ask for the conventions editor; frames may interleave before it
            write_msg(&mut conn, &ClientMsg::Key(key('e'))).unwrap();
            let mut editor_path = None;
            for _ in 0..100 {
                match read_msg::<_, ServerMsg>(&mut conn).unwrap().unwrap() {
                    ServerMsg::RunEditor { path } => {
                        editor_path = Some(path);
                        break;
                    }
                    _ => continue,
                }
            }
            assert!(
                editor_path.unwrap().ends_with("conventions.md"),
                "RunEditor should carry the conventions path"
            );
            write_msg(&mut conn, &ClientMsg::EditorDone).unwrap();

            write_msg(
                &mut conn,
                &ClientMsg::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 5,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                }),
            )
            .unwrap();

            // resize: every client reattaches with a full frame of the new size
            write_msg(
                &mut conn,
                &ClientMsg::Resize {
                    cols: 100,
                    rows: 30,
                },
            )
            .unwrap();
            let mut got_resized_full = false;
            for _ in 0..100 {
                if let Some(ServerMsg::FrameFull { cols, rows, .. }) =
                    read_msg::<_, ServerMsg>(&mut conn).unwrap()
                    && (cols, rows) == (100, 30)
                {
                    got_resized_full = true;
                    break;
                }
            }
            assert!(got_resized_full, "no FrameFull after resize");

            // quit key: daemon answers Detach and drops the client
            write_msg(&mut conn, &ClientMsg::Key(key('q'))).unwrap();
            assert!(
                matches!(
                    read_msg::<_, ServerMsg>(&mut conn).unwrap(),
                    Some(ServerMsg::Detach)
                ),
                "no Detach after q"
            );
            drop(conn);

            // daemon survives the detach: a new client attaches and shuts it down
            let mut conn = connect();
            write_msg(&mut conn, &ClientMsg::Resize { cols: 80, rows: 24 }).unwrap();
            assert!(
                matches!(
                    read_msg::<_, ServerMsg>(&mut conn).unwrap(),
                    Some(ServerMsg::FrameFull { .. })
                ),
                "second client did not get a FrameFull"
            );
            // a state change while attached (and not pending) arrives as a diff
            write_msg(
                &mut conn,
                &ClientMsg::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            )
            .unwrap();
            assert!(
                matches!(
                    read_msg::<_, ServerMsg>(&mut conn).unwrap(),
                    Some(ServerMsg::FrameDiff(_))
                ),
                "tab change should arrive as FrameDiff"
            );
            // idle while attached: the daemon must hit its 16ms tick with an
            // empty diff and keep the connection silent
            std::thread::sleep(Duration::from_millis(80));
            write_msg(&mut conn, &ClientMsg::Shutdown).unwrap();
        });

        serve(&sock, app(), 80, 24).unwrap();
        client.join().unwrap();
        assert!(!sock.exists());
    }

    #[test]
    fn serve_accepts_client_and_sends_full_frame() {
        use crate::protocol::{ClientMsg, ServerMsg, read_msg, write_msg};
        use std::os::unix::net::UnixStream;

        let mut sock = std::env::temp_dir();
        sock.push(format!(
            "jaum-serve-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let sock_c = sock.clone();

        // the client runs on a thread; `serve` (App owner) stays on the current thread.
        let client = std::thread::spawn(move || {
            // wait for the socket to come up
            let mut conn = loop {
                if let Ok(c) = UnixStream::connect(&sock_c) {
                    break c;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            write_msg(&mut conn, &ClientMsg::Resize { cols: 80, rows: 24 }).unwrap();
            // at least one FrameFull should arrive
            let got_full = match read_msg::<_, ServerMsg>(&mut conn).unwrap() {
                Some(ServerMsg::FrameFull { cols, rows, cells }) => {
                    assert_eq!((cols, rows), (80, 24));
                    assert_eq!(cells.len(), 80 * 24);
                    true
                }
                _ => false,
            };
            write_msg(&mut conn, &ClientMsg::Shutdown).unwrap();
            got_full
        });

        serve(&sock, app(), 80, 24).unwrap();
        assert!(client.join().unwrap(), "client did not receive FrameFull");
        assert!(!sock.exists(), "socket should be removed on shutdown");
    }
}
