//! Protocolo do daemon ⇄ cliente. Mensagens serde sobre um stream (unix socket),
//! com framing length-prefixed (4 bytes big-endian + payload JSON).
//!
//! O daemon é dono do estado e renderiza tudo num `Buffer`; manda só os deltas de
//! células (`FrameDiff`) — o cliente é render puro.

use std::io::{self, Read, Write};

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::style::{Color, Modifier};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;

/// Uma célula da tela na posição (x, y). Tudo serializável graças às features
/// `serde` da ratatui (Color/Modifier).
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

/// Cliente → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Uma tecla digitada (encaminhada pro `handle_key` do daemon).
    Key(KeyEvent),
    /// Um evento de mouse (scroll/click) sobre o pane do terminal.
    Mouse(MouseEvent),
    /// Tamanho do terminal do cliente (last-writer-wins no daemon).
    Resize { cols: u16, rows: u16 },
    /// O cliente terminou de rodar o `$EDITOR` pedido via `RunEditor`.
    EditorDone,
    /// Derruba o daemon (encerra sessões e sai).
    Shutdown,
}

/// Daemon → cliente.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Tela completa (no attach e em cada resize).
    FrameFull { cols: u16, rows: u16, cells: Vec<WireCell> },
    /// Só as células que mudaram desde o último frame.
    FrameDiff(Vec<WireCell>),
    /// O daemon pede que o cliente desanexe (ex.: o usuário apertou `q`).
    Detach,
    /// O daemon pede que o cliente rode `$EDITOR` neste caminho (passo interativo).
    RunEditor { path: String },
}

/// Escreve uma mensagem com framing length-prefixed.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(payload.len()).map_err(io::Error::other)?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Lê uma mensagem (bloqueante). `Ok(None)` em EOF limpo.
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
            ClientMsg::Resize { cols: 120, rows: 40 },
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
            // compara via re-serialização (ClientMsg não deriva PartialEq)
            assert_eq!(
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(expected).unwrap()
            );
        }
        // EOF limpo
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
            _ => panic!("tipo errado"),
        }
    }
}
