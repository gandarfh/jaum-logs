//! TUI client: connects to the daemon, renders the `DomainSnapshot`s it
//! receives and maps key presses to `Intent`s. The daemon owns all state; the
//! client owns presentation (layout, colors, key bindings).

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::keymap;
use crate::protocol::{
    ClientMsg, Device, DeviceKind, DomainSnapshot, PROTOCOL_VERSION, ServerMsg, read_msg, write_msg,
};
use crate::tui;

/// Liveness cadence (the daemon tracks per-device RTT from these).
const PING_EVERY: Duration = Duration::from_secs(2);
/// Idle repaint cadence (keeps relative ages like "active · 3m" fresh).
const REDRAW_EVERY: Duration = Duration::from_secs(1);

/// Events from the socket reader thread to the main loop.
pub(crate) enum SrvEvent {
    /// A new snapshot landed in the shared slot.
    Snapshot,
    Detach,
    Editor(String),
    Pong {
        seq: u64,
    },
}

/// Terminal side effects used by the client loop. The production impl wraps the
/// real terminal; keeping it behind a trait keeps the loop free of tty state.
pub(crate) trait Ui {
    /// Paint the snapshot.
    fn draw(&mut self, snap: &DomainSnapshot) -> Result<()>;
    /// Next input event, if any arrives within `timeout`.
    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>>;
    /// Suspend the TUI, run `$EDITOR` on `path` and resume.
    fn run_editor(&mut self, path: &str) -> Result<()>;
}

/// Machine hostname (identifies the device better than $USER: two terminals
/// of the same user on different machines stay distinguishable).
pub(crate) fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes a NUL-terminated name into the buffer we own.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|b| *b == 0)?;
    let name = String::from_utf8_lossy(&buf[..len]).into_owned();
    (!name.is_empty()).then_some(name)
}

/// This client as a `Device` for the handshake.
fn local_device() -> Device {
    Device {
        name: hostname()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "terminal".into()),
        kind: DeviceKind::Terminal,
    }
}

/// Sends `Hello` and waits for the daemon's verdict. Returns the device id.
pub(crate) fn hello<W: Write, R: Read>(w: &mut W, r: &mut R, device: Device) -> Result<u64> {
    write_msg(
        w,
        &ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            device,
            token: None,
        },
    )?;
    match read_msg::<_, ServerMsg>(r)? {
        Some(ServerMsg::Welcome { device_id, .. }) => Ok(device_id),
        Some(ServerMsg::Rejected { reason }) => bail!("daemon refused the connection: {reason}"),
        _ => bail!("daemon closed the connection during the handshake"),
    }
}

/// Connect to the daemon and run the client loop until detach.
pub fn run(sock: &Path) -> Result<()> {
    let mut write_half = UnixStream::connect(sock)?;
    let read_half = write_half.try_clone()?;
    let mut reader = BufReader::new(read_half);

    hello(&mut write_half, &mut reader, local_device())?;

    let snap: Arc<Mutex<Option<DomainSnapshot>>> = Arc::new(Mutex::new(None));
    let (stx, srx) = channel::<SrvEvent>();
    spawn_reader(reader, snap.clone(), stx);

    let mut ui = TerminalUi {
        terminal: ratatui::init(),
    };
    let res = client_loop(&mut ui, &mut write_half, &srx, &snap, PING_EVERY);
    ratatui::restore();
    res
}

/// Main loop: apply server events, redraw, forward input as intents and keep
/// the liveness pings flowing.
pub(crate) fn client_loop<W: Write>(
    ui: &mut dyn Ui,
    write_half: &mut W,
    srx: &Receiver<SrvEvent>,
    snap: &Arc<Mutex<Option<DomainSnapshot>>>,
    ping_every: Duration,
) -> Result<()> {
    let mut needs_redraw = false;
    let mut last_draw = Instant::now();
    let mut next_seq: u64 = 0;
    let mut ping_sent: Option<(u64, Instant)> = None;
    // None = never pinged, so the first loop iteration pings right away.
    // (Subtracting the interval from Instant::now() instead can underflow on
    // a freshly booted system, where the monotonic clock is near zero.)
    let mut last_ping: Option<Instant> = None;
    let mut last_rtt_ms: Option<u64> = None;
    loop {
        // 1) server events
        while let Ok(ev) = srx.try_recv() {
            match ev {
                SrvEvent::Snapshot => needs_redraw = true,
                SrvEvent::Detach => return Ok(()),
                SrvEvent::Editor(path) => {
                    ui.run_editor(&path)?;
                    write_msg(write_half, &ClientMsg::EditorDone)?;
                    needs_redraw = true;
                }
                SrvEvent::Pong { seq } => {
                    if let Some((sent_seq, at)) = ping_sent
                        && sent_seq == seq
                    {
                        last_rtt_ms = Some(at.elapsed().as_millis() as u64);
                        ping_sent = None;
                    }
                }
            }
        }

        // 2) liveness ping (reports the RTT measured on the previous pong)
        if last_ping.is_none_or(|t| t.elapsed() >= ping_every) {
            write_msg(
                write_half,
                &ClientMsg::Ping {
                    seq: next_seq,
                    last_rtt_ms,
                },
            )?;
            ping_sent = Some((next_seq, Instant::now()));
            next_seq += 1;
            last_ping = Some(Instant::now());
        }

        // 3) redraw from the shared snapshot (or periodically, for the ages)
        if needs_redraw || last_draw.elapsed() >= REDRAW_EVERY {
            let current = snap.lock().unwrap().clone();
            if let Some(s) = current {
                ui.draw(&s)?;
                last_draw = Instant::now();
            }
            needs_redraw = false;
        }

        // 4) local input -> intents
        if let Some(ev) = ui.poll_event(Duration::from_millis(10))? {
            match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    let ctx = {
                        snap.lock()
                            .unwrap()
                            .as_ref()
                            .map(keymap::KeyCtx::from_snapshot)
                    };
                    match ctx {
                        Some(ctx) => {
                            if let Some(intent) = keymap::map_key(&ctx, k) {
                                let intent = keymap::with_local_prefill(intent);
                                write_msg(write_half, &ClientMsg::Intent(intent))?;
                            }
                        }
                        // no snapshot yet, so no key context: still honor the
                        // quit keys locally, or a silent daemon would leave the
                        // user stuck with only kill -9 as an exit.
                        None => {
                            let ctrl_c = k.code == crossterm::event::KeyCode::Char('c')
                                && k.modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL);
                            if k.code == crossterm::event::KeyCode::Char('q') || ctrl_c {
                                return Ok(());
                            }
                        }
                    }
                }
                Event::Resize(_, _) => needs_redraw = true,
                _ => {}
            }
        }
    }
}

/// Real terminal: crossterm input + ratatui painting. The editor step is the
/// only interactive suspend/resume delegated to the client.
struct TerminalUi {
    terminal: DefaultTerminal,
}

impl Ui for TerminalUi {
    fn draw(&mut self, snap: &DomainSnapshot) -> Result<()> {
        self.terminal.draw(|f| tui::render(f, snap, None))?;
        Ok(())
    }

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>> {
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }

    fn run_editor(&mut self, path: &str) -> Result<()> {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        ratatui::restore();
        let _ = std::process::Command::new(editor).arg(path).status();
        self.terminal = ratatui::init();
        let _ = self.terminal.clear();
        Ok(())
    }
}

/// Reader thread: stores snapshots in the shared slot and signals the loop.
/// Session events are dropped (this client has no session panel consuming
/// them); everything else maps 1:1 to a loop event.
pub(crate) fn spawn_reader(
    mut reader: BufReader<UnixStream>,
    snap: Arc<Mutex<Option<DomainSnapshot>>>,
    stx: Sender<SrvEvent>,
) {
    thread::spawn(move || {
        loop {
            match read_msg::<_, ServerMsg>(&mut reader) {
                Ok(Some(ServerMsg::Snapshot(s))) => {
                    *snap.lock().unwrap() = Some(*s);
                    let _ = stx.send(SrvEvent::Snapshot);
                }
                Ok(Some(ServerMsg::Session(_))) => {}
                Ok(Some(ServerMsg::Pong { seq, .. })) => {
                    let _ = stx.send(SrvEvent::Pong { seq });
                }
                Ok(Some(ServerMsg::RunEditor { path })) => {
                    let _ = stx.send(SrvEvent::Editor(path));
                }
                // late handshake frames are meaningless mid-session; skip them.
                Ok(Some(ServerMsg::Welcome { .. })) | Ok(Some(ServerMsg::Rejected { .. })) => {}
                Ok(Some(ServerMsg::Detach)) | Ok(None) | Err(_) => {
                    let _ = stx.send(SrvEvent::Detach);
                    break;
                }
            }
        }
    });
}

// Unit tests live in-crate (not under tests/) so llvm-cov attributes the
// exercised lines to this file.
#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
