//! Cliente TUI: render puro. Conecta no daemon, encaminha teclas/resize e desenha
//! os frames que chegam. Toda a lógica de estado vive no daemon — aqui só se pinta
//! o `Buffer` recebido. `q`/`Ctrl+C` viram detach (o daemon manda `Detach`).

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Sender, channel};
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

/// Eventos vindos da thread leitora do socket para o loop principal.
enum SrvEvent {
    Redraw,
    Detach,
    Editor(String),
}

/// Conecta no daemon e roda o loop do cliente até detach.
pub fn run(sock: &Path) -> Result<()> {
    let mut write_half = UnixStream::connect(sock)?;
    let read_half = write_half.try_clone()?;

    let screen = Arc::new(Mutex::new(Buffer::empty(Rect::new(0, 0, 1, 1))));
    let (stx, srx) = channel::<SrvEvent>();
    spawn_reader(read_half, screen.clone(), stx);

    // handshake: manda o tamanho atual para o daemon renderizar e devolver um full.
    let (cols, rows) = crossterm::terminal::size()?;
    write_msg(&mut write_half, &ClientMsg::Resize { cols, rows })?;

    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let res = client_loop(&mut terminal, &mut write_half, &srx, &screen);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    res
}

/// Loop principal: aplica eventos do servidor, redesenha e encaminha input.
fn client_loop(
    terminal: &mut DefaultTerminal,
    write_half: &mut UnixStream,
    srx: &std::sync::mpsc::Receiver<SrvEvent>,
    screen: &Arc<Mutex<Buffer>>,
) -> Result<()> {
    let mut needs_redraw = false;
    loop {
        // 1) eventos do servidor
        while let Ok(ev) = srx.try_recv() {
            match ev {
                SrvEvent::Redraw => needs_redraw = true,
                SrvEvent::Detach => return Ok(()),
                SrvEvent::Editor(path) => {
                    run_editor(terminal, write_half, &path)?;
                    needs_redraw = true;
                }
            }
        }

        // 2) redesenha a partir do buffer compartilhado
        if needs_redraw {
            let buf = screen.lock().unwrap().clone();
            terminal.draw(|f| blit(f, &buf))?;
            needs_redraw = false;
        }

        // 3) input local -> daemon
        if event::poll(Duration::from_millis(10))? {
            match event::read()? {
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

/// Suspende a TUI, roda o `$EDITOR` no caminho pedido e retoma, pedindo um frame
/// completo de volta. É o único passo interativo delegado ao cliente.
fn run_editor(terminal: &mut DefaultTerminal, write_half: &mut UnixStream, path: &str) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    ratatui::restore();
    let _ = std::process::Command::new(editor).arg(path).status();
    *terminal = ratatui::init();
    let _ = terminal.clear();
    write_msg(write_half, &ClientMsg::EditorDone)?;
    let (cols, rows) = crossterm::terminal::size()?;
    write_msg(write_half, &ClientMsg::Resize { cols, rows })?;
    Ok(())
}

/// Copia o buffer do daemon no frame do terminal (recorta na sobreposição).
fn blit(f: &mut Frame, src: &Buffer) {
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

/// Thread leitora: aplica frames no buffer compartilhado e sinaliza o loop.
fn spawn_reader(read_half: UnixStream, screen: Arc<Mutex<Buffer>>, stx: Sender<SrvEvent>) {
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
