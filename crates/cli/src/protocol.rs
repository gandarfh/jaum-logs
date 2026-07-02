//! Daemon ⇄ client protocol. Serde messages over a stream (unix socket), with
//! length-prefixed framing (4-byte big-endian length + JSON payload).
//!
//! The daemon owns the state and renders everything into a `Buffer`; it sends only
//! the cell deltas (`FrameDiff`) — the client is pure render.

use std::io::{self, Read, Write};

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::style::{Color, Modifier};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A screen cell at position (x, y). Fully serializable thanks to ratatui's
/// `serde` features (Color/Modifier).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCell {
    pub x: u16,
    pub y: u16,
    pub sym: String,
    pub fg: Color,
    pub bg: Color,
    pub underline: Color,
    pub mods: Modifier,
}

/// Client → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// A key press (forwarded to the daemon's `handle_key`).
    Key(KeyEvent),
    /// A mouse event (scroll/click) over the terminal pane.
    Mouse(MouseEvent),
    /// The client's terminal size (last-writer-wins in the daemon).
    Resize { cols: u16, rows: u16 },
    /// The client finished running the `$EDITOR` requested via `RunEditor`.
    EditorDone,
    /// Shut the daemon down (stop sessions and exit).
    Shutdown,
}

/// Daemon → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Full screen (on attach and on every resize).
    FrameFull {
        cols: u16,
        rows: u16,
        cells: Vec<WireCell>,
    },
    /// Only the cells that changed since the last frame.
    FrameDiff(Vec<WireCell>),
    /// The daemon asks the client to detach (e.g. the user pressed `q`).
    Detach,
    /// The daemon asks the client to run `$EDITOR` on this path (interactive step).
    RunEditor { path: String },
}

/// Write a message with length-prefixed framing.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(payload.len()).map_err(io::Error::other)?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Read a message (blocking). `Ok(None)` on clean EOF.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    let msg = serde_json::from_slice(&payload).map_err(io::Error::other)?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn framing_roundtrip_client_msg() {
        let msgs = vec![
            ClientMsg::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            ClientMsg::Resize {
                cols: 120,
                rows: 40,
            },
            ClientMsg::EditorDone,
            ClientMsg::Shutdown,
        ];
        let mut buf: Vec<u8> = Vec::new();
        for m in &msgs {
            write_msg(&mut buf, m).unwrap();
        }
        let mut cur = std::io::Cursor::new(buf);
        for expected in &msgs {
            let got: ClientMsg = read_msg(&mut cur).unwrap().unwrap();
            // compare via re-serialization (ClientMsg does not derive PartialEq)
            assert_eq!(
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(expected).unwrap()
            );
        }
        // clean EOF
        let end: Option<ClientMsg> = read_msg(&mut cur).unwrap();
        assert!(end.is_none());
    }

    #[test]
    fn wirecell_roundtrip() {
        let c = WireCell {
            x: 3,
            y: 7,
            sym: "◆".into(),
            fg: Color::Rgb(180, 142, 255),
            bg: Color::Reset,
            underline: Color::Reset,
            mods: Modifier::BOLD | Modifier::ITALIC,
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &ServerMsg::FrameDiff(vec![c.clone()])).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let got: ServerMsg = read_msg(&mut cur).unwrap().unwrap();
        match got {
            ServerMsg::FrameDiff(cells) => assert_eq!(cells, vec![c]),
            _ => panic!("wrong type"),
        }
    }
}
