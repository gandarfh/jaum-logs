//! TUI client: pure render. Connects to the daemon, forwards keys/resize and draws
//! incoming frames. All state logic lives in the daemon — here we only paint the
//! received `Buffer`. `q`/`Ctrl+C` become detach (the daemon sends `Detach`).

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

use crate::backend::apply_cells;
use crate::protocol::{ClientMsg, ServerMsg, read_msg, write_msg};

/// Events from the socket reader thread to the main loop.
pub(crate) enum SrvEvent {
    Redraw,
    Detach,
    Editor(String),
}

/// Terminal side effects used by the client loop. The production impl wraps the
/// real terminal; keeping it behind a trait keeps the loop free of tty state.
pub(crate) trait Ui {
    /// Current terminal size (cols, rows).
    fn size(&self) -> Result<(u16, u16)>;
    /// Paint the shared screen buffer.
    fn draw(&mut self, screen: &Buffer) -> Result<()>;
    /// Next input event, if any arrives within `timeout`.
    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>>;
    /// Suspend the TUI, run `$EDITOR` on `path` and resume.
    fn run_editor(&mut self, path: &str) -> Result<()>;
}

/// Connect to the daemon and run the client loop until detach.
pub fn run(sock: &Path) -> Result<()> {
    let mut write_half = UnixStream::connect(sock)?;
    let read_half = write_half.try_clone()?;

    let screen = Arc::new(Mutex::new(Buffer::empty(Rect::new(0, 0, 1, 1))));
    let (stx, srx) = channel::<SrvEvent>();
    spawn_reader(read_half, screen.clone(), stx);

    // handshake: send the current size so the daemon renders and returns a full frame.
    let (cols, rows) = crossterm::terminal::size()?;
    write_msg(&mut write_half, &ClientMsg::Resize { cols, rows })?;

    let mut ui = TerminalUi {
        terminal: ratatui::init(),
    };
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let res = client_loop(&mut ui, &mut write_half, &srx, &screen);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    res
}

/// Main loop: apply server events, redraw, and forward input.
pub(crate) fn client_loop<W: Write>(
    ui: &mut dyn Ui,
    write_half: &mut W,
    srx: &Receiver<SrvEvent>,
    screen: &Arc<Mutex<Buffer>>,
) -> Result<()> {
    let mut needs_redraw = false;
    loop {
        // 1) server events
        while let Ok(ev) = srx.try_recv() {
            match ev {
                SrvEvent::Redraw => needs_redraw = true,
                SrvEvent::Detach => return Ok(()),
                SrvEvent::Editor(path) => {
                    ui.run_editor(&path)?;
                    write_msg(write_half, &ClientMsg::EditorDone)?;
                    let (cols, rows) = ui.size()?;
                    write_msg(write_half, &ClientMsg::Resize { cols, rows })?;
                    needs_redraw = true;
                }
            }
        }

        // 2) redraw from the shared buffer
        if needs_redraw {
            let buf = screen.lock().unwrap().clone();
            ui.draw(&buf)?;
            needs_redraw = false;
        }

        // 3) local input -> daemon
        if let Some(ev) = ui.poll_event(Duration::from_millis(10))? {
            match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    write_msg(write_half, &ClientMsg::Key(k))?;
                }
                Event::Mouse(m) => {
                    write_msg(write_half, &ClientMsg::Mouse(m))?;
                }
                Event::Resize(cols, rows) => {
                    write_msg(write_half, &ClientMsg::Resize { cols, rows })?;
                }
                _ => {}
            }
        }
    }
}

/// Real terminal: crossterm input + ratatui painting. The editor step is the only
/// interactive suspend/resume delegated to the client.
struct TerminalUi {
    terminal: DefaultTerminal,
}

impl Ui for TerminalUi {
    fn size(&self) -> Result<(u16, u16)> {
        Ok(crossterm::terminal::size()?)
    }

    fn draw(&mut self, screen: &Buffer) -> Result<()> {
        self.terminal.draw(|f| blit(f, screen))?;
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

/// Copy the daemon's buffer into the terminal frame (clipped to the overlap).
pub(crate) fn blit(f: &mut Frame, src: &Buffer) {
    let dst = f.buffer_mut();
    let (da, sa) = (dst.area, src.area);
    let w = da.width.min(sa.width);
    let h = da.height.min(sa.height);
    for y in 0..h {
        for x in 0..w {
            if let Some(c) = src.cell(Position::new(sa.x + x, sa.y + y)).cloned()
                && let Some(d) = dst.cell_mut(Position::new(da.x + x, da.y + y))
            {
                *d = c;
            }
        }
    }
}

/// Reader thread: applies frames to the shared buffer and signals the loop.
pub(crate) fn spawn_reader(
    read_half: UnixStream,
    screen: Arc<Mutex<Buffer>>,
    stx: Sender<SrvEvent>,
) {
    thread::spawn(move || {
        let mut r = BufReader::new(read_half);
        loop {
            match read_msg::<_, ServerMsg>(&mut r) {
                Ok(Some(ServerMsg::FrameFull { cols, rows, cells })) => {
                    {
                        let mut b = screen.lock().unwrap();
                        *b = Buffer::empty(Rect::new(0, 0, cols, rows));
                        apply_cells(&mut b, &cells);
                    }
                    let _ = stx.send(SrvEvent::Redraw);
                }
                Ok(Some(ServerMsg::FrameDiff(cells))) => {
                    apply_cells(&mut screen.lock().unwrap(), &cells);
                    let _ = stx.send(SrvEvent::Redraw);
                }
                Ok(Some(ServerMsg::RunEditor { path })) => {
                    let _ = stx.send(SrvEvent::Editor(path));
                }
                Ok(Some(ServerMsg::Detach)) | Ok(None) | Err(_) => {
                    let _ = stx.send(SrvEvent::Detach);
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WireCell;
    use std::collections::VecDeque;
    use std::io::{self, Cursor};
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};

    fn wc(x: u16, y: u16, sym: &str) -> WireCell {
        WireCell {
            x,
            y,
            sym: sym.into(),
            fg: Color::Reset,
            bg: Color::Reset,
            underline: Color::Reset,
            mods: Modifier::empty(),
        }
    }

    fn empty_screen() -> Arc<Mutex<Buffer>> {
        Arc::new(Mutex::new(Buffer::empty(Rect::new(0, 0, 1, 1))))
    }

    fn recv(srx: &Receiver<SrvEvent>) -> SrvEvent {
        srx.recv_timeout(Duration::from_secs(5)).expect("event")
    }

    // --- spawn_reader --------------------------------------------------------

    #[test]
    fn reader_applies_frames_and_forwards_editor_and_detach() {
        let (client_end, mut server_end) = UnixStream::pair().unwrap();
        let screen = empty_screen();
        let (stx, srx) = channel();
        spawn_reader(client_end, screen.clone(), stx);

        write_msg(
            &mut server_end,
            &ServerMsg::FrameFull {
                cols: 4,
                rows: 2,
                cells: vec![wc(1, 0, "x")],
            },
        )
        .unwrap();
        assert!(matches!(recv(&srx), SrvEvent::Redraw));
        {
            let b = screen.lock().unwrap();
            assert_eq!(b.area, Rect::new(0, 0, 4, 2));
            assert_eq!(b.cell(Position::new(1, 0)).unwrap().symbol(), "x");
        }

        write_msg(&mut server_end, &ServerMsg::FrameDiff(vec![wc(2, 1, "y")])).unwrap();
        assert!(matches!(recv(&srx), SrvEvent::Redraw));
        assert_eq!(
            screen
                .lock()
                .unwrap()
                .cell(Position::new(2, 1))
                .unwrap()
                .symbol(),
            "y"
        );

        write_msg(
            &mut server_end,
            &ServerMsg::RunEditor {
                path: "/tmp/conventions.md".into(),
            },
        )
        .unwrap();
        assert!(matches!(recv(&srx), SrvEvent::Editor(p) if p == "/tmp/conventions.md"));

        write_msg(&mut server_end, &ServerMsg::Detach).unwrap();
        assert!(matches!(recv(&srx), SrvEvent::Detach));
    }

    #[test]
    fn reader_treats_eof_as_detach() {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let (stx, srx) = channel();
        spawn_reader(client_end, empty_screen(), stx);
        drop(server_end);
        assert!(matches!(recv(&srx), SrvEvent::Detach));
    }

    #[test]
    fn reader_treats_bad_frame_as_detach() {
        use std::io::Write as _;
        let (client_end, mut server_end) = UnixStream::pair().unwrap();
        let (stx, srx) = channel();
        spawn_reader(client_end, empty_screen(), stx);
        server_end.write_all(&4u32.to_be_bytes()).unwrap();
        server_end.write_all(b"zzzz").unwrap();
        server_end.flush().unwrap();
        assert!(matches!(recv(&srx), SrvEvent::Detach));
    }

    // --- blit -----------------------------------------------------------------

    fn marked(area: Rect, sym: &str) -> Buffer {
        let mut b = Buffer::empty(area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                b.cell_mut(Position::new(x, y)).unwrap().set_symbol(sym);
            }
        }
        b
    }

    #[test]
    fn blit_clips_larger_source_to_terminal() {
        let mut term = Terminal::new(TestBackend::new(3, 2)).unwrap();
        let src = marked(Rect::new(0, 0, 8, 8), "s");
        term.draw(|f| blit(f, &src)).unwrap();
        let out = term.backend().buffer();
        assert_eq!(out.cell(Position::new(0, 0)).unwrap().symbol(), "s");
        assert_eq!(out.cell(Position::new(2, 1)).unwrap().symbol(), "s");
    }

    #[test]
    fn blit_leaves_area_beyond_smaller_source_untouched() {
        let mut term = Terminal::new(TestBackend::new(6, 4)).unwrap();
        let src = marked(Rect::new(0, 0, 2, 2), "s");
        term.draw(|f| blit(f, &src)).unwrap();
        let out = term.backend().buffer();
        assert_eq!(out.cell(Position::new(1, 1)).unwrap().symbol(), "s");
        assert_eq!(out.cell(Position::new(3, 2)).unwrap().symbol(), " ");
    }

    // --- client_loop ------------------------------------------------------------

    /// Scripted Ui: returns queued input events; when the script runs out it
    /// sends `Detach` so the loop terminates.
    struct ScriptUi {
        polls: VecDeque<Event>,
        stx: Sender<SrvEvent>,
        drawn: Vec<Buffer>,
        editors: Vec<String>,
        size: (u16, u16),
        fail_editor: bool,
        fail_draw: bool,
    }

    impl ScriptUi {
        fn new(stx: Sender<SrvEvent>, polls: Vec<Event>) -> Self {
            Self {
                polls: polls.into(),
                stx,
                drawn: Vec::new(),
                editors: Vec::new(),
                size: (91, 31),
                fail_editor: false,
                fail_draw: false,
            }
        }
    }

    impl Ui for ScriptUi {
        fn size(&self) -> Result<(u16, u16)> {
            Ok(self.size)
        }

        fn draw(&mut self, screen: &Buffer) -> Result<()> {
            if self.fail_draw {
                anyhow::bail!("draw failed");
            }
            self.drawn.push(screen.clone());
            Ok(())
        }

        fn poll_event(&mut self, _timeout: Duration) -> Result<Option<Event>> {
            match self.polls.pop_front() {
                Some(ev) => Ok(Some(ev)),
                None => {
                    let _ = self.stx.send(SrvEvent::Detach);
                    Ok(None)
                }
            }
        }

        fn run_editor(&mut self, path: &str) -> Result<()> {
            if self.fail_editor {
                anyhow::bail!("editor failed");
            }
            self.editors.push(path.to_string());
            Ok(())
        }
    }

    fn press(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn release(c: char) -> Event {
        let mut k = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        Event::Key(k)
    }

    fn mouse() -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn sent_msgs(bytes: Vec<u8>) -> Vec<ClientMsg> {
        let mut cur = Cursor::new(bytes);
        let mut out = Vec::new();
        while let Some(m) = read_msg::<_, ClientMsg>(&mut cur).unwrap() {
            out.push(m);
        }
        out
    }

    #[test]
    fn loop_redraws_and_forwards_input_until_detach() {
        let (stx, srx) = channel();
        let screen = empty_screen();
        {
            let mut b = screen.lock().unwrap();
            *b = Buffer::empty(Rect::new(0, 0, 3, 1));
            b.cell_mut(Position::new(0, 0)).unwrap().set_symbol("k");
        }
        stx.send(SrvEvent::Redraw).unwrap();

        let mut ui = ScriptUi::new(
            stx,
            vec![
                press('a'),
                release('a'), // ignored: not a Press
                mouse(),
                Event::Resize(50, 20),
                Event::FocusGained, // ignored: unhandled variant
            ],
        );
        let mut wire = Vec::new();
        client_loop(&mut ui, &mut wire, &srx, &screen).unwrap();

        assert_eq!(ui.drawn.len(), 1);
        assert_eq!(ui.drawn[0].cell(Position::new(0, 0)).unwrap().symbol(), "k");

        let msgs = sent_msgs(wire);
        assert_eq!(msgs.len(), 3, "release/focus must not be forwarded");
        assert!(matches!(msgs[0], ClientMsg::Key(k) if k.code == KeyCode::Char('a')));
        assert!(matches!(msgs[1], ClientMsg::Mouse(m) if m.row == 4));
        assert!(matches!(msgs[2], ClientMsg::Resize { cols: 50, rows: 20 }));
    }

    #[test]
    fn loop_runs_editor_then_reports_done_and_size() {
        let (stx, srx) = channel();
        let screen = empty_screen();
        stx.send(SrvEvent::Editor("/tmp/conv.md".into())).unwrap();

        let mut ui = ScriptUi::new(stx, Vec::new());
        let mut wire = Vec::new();
        client_loop(&mut ui, &mut wire, &srx, &screen).unwrap();

        assert_eq!(ui.editors, vec!["/tmp/conv.md".to_string()]);
        assert_eq!(ui.drawn.len(), 1, "editor return forces a redraw");
        let msgs = sent_msgs(wire);
        assert!(matches!(msgs[0], ClientMsg::EditorDone));
        assert!(matches!(msgs[1], ClientMsg::Resize { cols: 91, rows: 31 }));
    }

    #[test]
    fn loop_propagates_editor_failure() {
        let (stx, srx) = channel();
        let screen = empty_screen();
        stx.send(SrvEvent::Editor("/tmp/conv.md".into())).unwrap();
        let mut ui = ScriptUi::new(stx, Vec::new());
        ui.fail_editor = true;
        let mut wire = Vec::new();
        assert!(client_loop(&mut ui, &mut wire, &srx, &screen).is_err());
    }

    #[test]
    fn loop_propagates_draw_failure() {
        let (stx, srx) = channel();
        let screen = empty_screen();
        stx.send(SrvEvent::Redraw).unwrap();
        let mut ui = ScriptUi::new(stx, Vec::new());
        ui.fail_draw = true;
        let mut wire = Vec::new();
        assert!(client_loop(&mut ui, &mut wire, &srx, &screen).is_err());
    }

    /// Accepts writes but fails on flush, like a socket whose peer vanished.
    struct FailWriter;

    impl Write for FailWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("broken pipe"))
        }
    }

    #[test]
    fn loop_propagates_socket_write_failure() {
        let (stx, srx) = channel();
        let screen = empty_screen();
        let mut ui = ScriptUi::new(stx, vec![press('a')]);
        assert!(client_loop(&mut ui, &mut FailWriter, &srx, &screen).is_err());
    }
}
