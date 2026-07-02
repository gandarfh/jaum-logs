//! Ponte entre o `Buffer` da ratatui (que o daemon renderiza) e as `WireCell`
//! do protocolo. O daemon manda diffs; o cliente aplica num `Buffer` local.

use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::layout::Position;

use crate::protocol::WireCell;

/// Converte uma célula numa `WireCell` posicionada.
fn to_wire(x: u16, y: u16, c: &Cell) -> WireCell {
    WireCell {
        x,
        y,
        sym: c.symbol().to_string(),
        fg: c.fg,
        bg: c.bg,
        underline: c.underline_color,
        mods: c.modifier,
    }
}

/// Aplica uma `WireCell` numa célula da ratatui.
fn from_wire(w: &WireCell) -> Cell {
    let mut c = Cell::default();
    c.set_symbol(&w.sym);
    c.fg = w.fg;
    c.bg = w.bg;
    c.underline_color = w.underline;
    c.modifier = w.mods;
    c
}

/// Todas as células do buffer (frame completo — usado no attach e no resize).
pub fn full_cells(buf: &Buffer) -> Vec<WireCell> {
    let area = buf.area;
    let mut out = Vec::with_capacity((area.width as usize) * (area.height as usize));
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell(Position::new(x, y)) {
                out.push(to_wire(x, y, c));
            }
        }
    }
    out
}

/// Só as células que mudaram de `prev` para `next` (assume mesma área).
pub fn diff_cells(prev: &Buffer, next: &Buffer) -> Vec<WireCell> {
    if prev.area != next.area {
        return full_cells(next);
    }
    let area = next.area;
    let mut out = Vec::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let p = prev.cell(Position::new(x, y));
            let n = next.cell(Position::new(x, y));
            if let (Some(p), Some(n)) = (p, n)
                && p != n
            {
                out.push(to_wire(x, y, n));
            }
        }
    }
    out
}

/// Aplica células recebidas num buffer local (cliente). Ignora as fora da área.
pub fn apply_cells(buf: &mut Buffer, cells: &[WireCell]) {
    for w in cells {
        let pos = Position::new(w.x, w.y);
        if buf.area.contains(pos)
            && let Some(slot) = buf.cell_mut(pos)
        {
            *slot = from_wire(w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    fn buf(area: Rect) -> Buffer {
        Buffer::empty(area)
    }

    #[test]
    fn full_cells_cobre_toda_a_area() {
        let b = buf(Rect::new(0, 0, 4, 3));
        assert_eq!(full_cells(&b).len(), 12);
    }

    #[test]
    fn diff_so_pega_celulas_alteradas() {
        let area = Rect::new(0, 0, 5, 2);
        let prev = buf(area);
        let mut next = buf(area);
        next.cell_mut(Position::new(2, 1)).unwrap().set_symbol("x");
        next.cell_mut(Position::new(0, 0)).unwrap().fg = Color::Red;
        let d = diff_cells(&prev, &next);
        assert_eq!(d.len(), 2);
        assert!(d.iter().any(|c| c.x == 2 && c.y == 1 && c.sym == "x"));
        assert!(d.iter().any(|c| c.x == 0 && c.y == 0 && c.fg == Color::Red));
    }

    #[test]
    fn apply_reconstroi_o_buffer() {
        let area = Rect::new(0, 0, 5, 2);
        let mut next = buf(area);
        next.cell_mut(Position::new(2, 1)).unwrap().set_symbol("x");
        next.cell_mut(Position::new(1, 0)).unwrap().fg = Color::Green;

        let cells = full_cells(&next);
        let mut client = buf(area);
        apply_cells(&mut client, &cells);
        assert_eq!(client, next);
    }

    #[test]
    fn diff_de_area_diferente_volta_full() {
        let prev = buf(Rect::new(0, 0, 2, 2));
        let next = buf(Rect::new(0, 0, 4, 4));
        assert_eq!(diff_cells(&prev, &next).len(), 16);
    }
}
