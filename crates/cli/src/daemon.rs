//! Daemon: sole owner of the state (`App`) and the PTYs. Exposes the domain as
//! `DomainSnapshot` broadcasts and applies client `Intent`s. No rendering
//! happens here; clients draw the snapshot themselves.

use std::collections::{HashMap, VecDeque};
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::app::App;
use crate::protocol::{
    ClientMsg, DaemonStatus, Device, DeviceStatus, DomainSnapshot, Intent, PROTOCOL_VERSION,
    ServerMsg, read_msg, write_msg,
};
use crate::snapshot::build_snapshot;

/// Idle tick: how often the daemon polls PTYs/jobs with no client events.
const TICK: Duration = Duration::from_millis(16);
/// Hard ceiling for one burst: even a continuous event stream must yield to
/// tick/broadcast at least this often.
const COALESCE_CAP: Duration = Duration::from_millis(100);
/// Window used to derive `snapshots_per_sec`.
const RATE_WINDOW: Duration = Duration::from_secs(1);

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

/// Side effects of applying one intent, targeted at the sender.
pub struct IntentEffects {
    /// The user asked to quit: detach THIS client (the daemon keeps running).
    pub detach: bool,
    /// The user asked to edit `conventions.md`: the client runs `$EDITOR` here.
    pub editor: Option<String>,
}

/// Daemon state: the `App` plus the last snapshot broadcast (for coalescing
/// and deduplication).
pub struct Daemon {
    app: App,
    last: Option<DomainSnapshot>,
}

impl Daemon {
    pub fn new(app: App) -> Self {
        Self { app, last: None }
    }

    /// Drains PTYs/jobs/toast and picks up external edits. PTYs keep their
    /// spawn size: the wire has no viewport/resize path while the session
    /// panel is a placeholder on socket clients.
    pub fn tick(&mut self) {
        self.app.drain_pty();
        self.app.drain_sidecar();
        self.app.tick_permissions();
        self.app.tick_sidecar_health();
        self.app.poll_job();
        self.app.tick_reload();
        self.app.tick_pr_sync();
        self.app.tick_toast();
    }

    /// Applies an intent and reports its per-client side effects.
    pub fn apply(&mut self, intent: Intent) -> IntentEffects {
        self.app.apply_intent(intent);
        // in the daemon "quit" means detaching the client; sessions keep running.
        let detach = std::mem::take(&mut self.app.should_quit);
        IntentEffects {
            detach,
            editor: self.take_editor_request(),
        }
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

    /// Last snapshot handed out by `snapshot_if_changed`.
    pub fn last_snapshot(&self) -> Option<&DomainSnapshot> {
        self.last.as_ref()
    }

    /// Snapshot only if the domain changed since the last one returned here.
    /// This is the coalescing core: however many events were applied since the
    /// previous call, at most ONE snapshot comes out.
    pub fn snapshot_if_changed(&mut self) -> Option<DomainSnapshot> {
        let snap = build_snapshot(&self.app);
        if self.last.as_ref() == Some(&snap) {
            return None;
        }
        self.last = Some(snap.clone());
        Some(snap)
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }
}

/// A connected client. `device` is set by a valid `Hello`; until then the
/// connection cannot send intents and receives no broadcasts.
struct Client {
    stream: UnixStream,
    device: Option<Device>,
    rtt_ms: Option<u64>,
}

/// Serve-loop state shared by the event handlers.
struct Server {
    daemon: Daemon,
    clients: HashMap<u64, Client>,
    running: bool,
    started: Instant,
    snap_times: VecDeque<Instant>,
}

impl Server {
    fn new(daemon: Daemon) -> Self {
        Self {
            daemon,
            clients: HashMap::new(),
            running: true,
            started: Instant::now(),
            snap_times: VecDeque::new(),
        }
    }

    /// Drops rate samples older than the window. Called on every push AND on
    /// every read, so the deque stays bounded even when no client pings.
    fn prune_snap_times(&mut self) {
        let now = Instant::now();
        while let Some(t) = self.snap_times.front() {
            if now.duration_since(*t) > RATE_WINDOW {
                self.snap_times.pop_front();
            } else {
                break;
            }
        }
    }

    /// Records a snapshot broadcast for the rate telemetry.
    fn note_snapshot(&mut self) {
        self.prune_snap_times();
        self.snap_times.push_back(Instant::now());
    }

    /// Telemetry for `Pong`: uptime, recent snapshot rate, connected devices.
    fn status(&mut self) -> DaemonStatus {
        self.prune_snap_times();
        let now = Instant::now();
        let mut devices: Vec<DeviceStatus> = self
            .clients
            .iter()
            .filter_map(|(id, c)| {
                c.device.as_ref().map(|d| DeviceStatus {
                    id: *id,
                    name: d.name.clone(),
                    kind: d.kind,
                    rtt_ms: c.rtt_ms,
                })
            })
            .collect();
        devices.sort_by_key(|d| d.id);
        DaemonStatus {
            uptime_ms: now.duration_since(self.started).as_millis() as u64,
            snapshots_per_sec: self.snap_times.len() as f64 / RATE_WINDOW.as_secs_f64(),
            devices,
        }
    }

    /// Sends to every handshaken client, dropping the ones that went away.
    fn broadcast(&mut self, msg: &ServerMsg) {
        let mut dead = Vec::new();
        for (id, c) in self.clients.iter_mut() {
            if c.device.is_none() {
                continue;
            }
            if write_msg(&mut c.stream, msg).is_err() {
                dead.push(*id);
            }
        }
        for id in dead {
            self.clients.remove(&id);
        }
    }

    /// Sends to one client; on failure the client is dropped.
    fn send_to(&mut self, id: u64, msg: &ServerMsg) {
        let dead = match self.clients.get_mut(&id) {
            Some(c) => write_msg(&mut c.stream, msg).is_err(),
            None => false,
        };
        if dead {
            self.clients.remove(&id);
        }
    }

    /// Removes a client AND closes its connection. Dropping the map entry is
    /// not enough: the acceptor's reader thread holds a clone of the fd, so
    /// only an explicit shutdown makes the peer see EOF.
    fn drop_client(&mut self, id: u64) {
        if let Some(c) = self.clients.remove(&id) {
            let _ = c.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Connect(id, stream) => {
                self.clients.insert(
                    id,
                    Client {
                        stream,
                        device: None,
                        rtt_ms: None,
                    },
                );
            }
            Event::Disconnect(id) => {
                self.clients.remove(&id);
            }
            Event::Client(
                id,
                ClientMsg::Hello {
                    protocol_version,
                    device,
                    token: _,
                },
            ) => {
                if protocol_version != PROTOCOL_VERSION {
                    self.send_to(
                        id,
                        &ServerMsg::Rejected {
                            reason: format!(
                                "incompatible protocol: daemon speaks v{PROTOCOL_VERSION}, client sent v{protocol_version}"
                            ),
                        },
                    );
                    self.drop_client(id);
                    return;
                }
                if self.clients.get(&id).is_some_and(|c| c.device.is_some()) {
                    self.send_to(
                        id,
                        &ServerMsg::Rejected {
                            reason: "handshake already completed".into(),
                        },
                    );
                    self.drop_client(id);
                    return;
                }
                // events buffered while nobody was attached are history, not
                // state: the first arrival bootstraps from the snapshot and
                // must not receive them on the next broadcast.
                let had_audience = self.clients.values().any(|c| c.device.is_some());
                if !had_audience {
                    let _ = self.daemon.app_mut().take_session_events();
                }
                if let Some(c) = self.clients.get_mut(&id) {
                    c.device = Some(device);
                }
                self.send_to(
                    id,
                    &ServerMsg::Welcome {
                        protocol_version: PROTOCOL_VERSION,
                        device_id: id,
                    },
                );
                // the fresh client needs the full state right away: broadcast
                // when the domain moved since the last snapshot, otherwise
                // replay the cached one to this client only (no dup for the
                // others, exactly one for the newcomer).
                match self.daemon.snapshot_if_changed() {
                    Some(snap) => {
                        self.note_snapshot();
                        self.broadcast(&ServerMsg::Snapshot(Box::new(snap)));
                    }
                    None => {
                        let cached = self
                            .daemon
                            .last_snapshot()
                            .cloned()
                            .expect("dedupe state exists when nothing changed");
                        self.send_to(id, &ServerMsg::Snapshot(Box::new(cached)));
                    }
                }
            }
            Event::Client(id, msg) => {
                // any traffic before a valid Hello is a protocol error, EXCEPT
                // Shutdown: `jaum shutdown` (and the installer swapping
                // binaries) must be able to stop a daemon of ANY protocol
                // version, so it cannot depend on a successful handshake.
                // Revisit when the auth token becomes enforced.
                if self.clients.get(&id).is_none_or(|c| c.device.is_none())
                    && !matches!(msg, ClientMsg::Shutdown)
                {
                    self.send_to(
                        id,
                        &ServerMsg::Rejected {
                            reason: "handshake required: send Hello first".into(),
                        },
                    );
                    self.drop_client(id);
                    return;
                }
                match msg {
                    ClientMsg::Intent(intent) => {
                        let fx = self.daemon.apply(intent);
                        if let Some(path) = fx.editor {
                            self.send_to(id, &ServerMsg::RunEditor { path });
                        }
                        if fx.detach {
                            self.send_to(id, &ServerMsg::Detach);
                            // plain remove (no socket shutdown): the client
                            // closes after reading Detach; an immediate
                            // shutdown here could turn an in-flight ping into
                            // EPIPE on the client side.
                            self.clients.remove(&id);
                        }
                    }
                    ClientMsg::Ping { seq, last_rtt_ms } => {
                        if let Some(c) = self.clients.get_mut(&id) {
                            c.rtt_ms = last_rtt_ms;
                        }
                        let status = self.status();
                        self.send_to(id, &ServerMsg::Pong { seq, status });
                    }
                    ClientMsg::EditorDone => self.daemon.editor_done(),
                    ClientMsg::Shutdown => self.running = false,
                    ClientMsg::Hello { .. } => unreachable!("handled above"),
                }
            }
        }
    }
}

/// Starts the server: accepts clients on `sock`, applies intents and
/// broadcasts coalesced snapshots. Blocks until it receives `Shutdown`. The
/// daemon runs on the calling thread (the `App`/PTY never crosses threads);
/// only sockets go to helper threads.
pub fn serve(sock: &Path, app: App) -> Result<()> {
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

    let mut server = Server::new(Daemon::new(app));

    while server.running {
        // 1) wait for events; once one lands, drain whatever else is already
        //    queued (no waiting for more) so an already-formed burst yields
        //    a single snapshot below, without delaying a lone event.
        match rx.recv_timeout(TICK) {
            Ok(ev) => {
                server.handle_event(ev);
                drain_burst(&rx, &mut server);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // 2) tick + broadcast (session events, then at most one snapshot)
        server.daemon.tick();
        for ev in server.daemon.app_mut().take_session_events() {
            server.broadcast(&ServerMsg::Session(ev));
        }
        let has_audience = server.clients.values().any(|c| c.device.is_some());
        if has_audience && let Some(snap) = server.daemon.snapshot_if_changed() {
            server.note_snapshot();
            server.broadcast(&ServerMsg::Snapshot(Box::new(snap)));
        }
    }

    // shutdown: stop the sessions (clean up worktrees) and remove the socket.
    server.daemon.app_mut().stop_all_sessions();
    let _ = std::fs::remove_file(sock);
    Ok(())
}

/// Drains events already queued, without waiting for more — a burst that
/// arrived together (already sitting in the channel) still yields a single
/// snapshot below, but a lone event pays no artificial delay. The cap keeps
/// a truly continuous stream (key auto-repeat, chatty client) from starving
/// `tick()` and the broadcasts indefinitely.
fn drain_burst(rx: &std::sync::mpsc::Receiver<Event>, server: &mut Server) {
    let deadline = Instant::now() + COALESCE_CAP;
    while server.running && Instant::now() < deadline {
        match rx.try_recv() {
            Ok(ev) => server.handle_event(ev),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                server.running = false;
            }
        }
    }
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

// Unit tests live in-crate (not under tests/) so llvm-cov attributes the
// exercised lines to this file.
#[cfg(test)]
#[path = "daemon_tests.rs"]
mod daemon_tests;
