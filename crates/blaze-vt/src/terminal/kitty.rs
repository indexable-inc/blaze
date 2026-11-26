//! A screen as kitty graphics sees it: [`GraphicsScreen`] for the storage (pins that follow the
//! cells, the grid in cells and pixels) and [`PlaceholderCells`] for the unicode placeholder
//! scan. The pixel size is the terminal's, so the adapter carries it beside the screen.

use blaze_own::planted::Planted;

use crate::input::mouse::Pixels;
use crate::kitty::storage::{ActivePoint, GraphicsScreen, Grid, PinId, ScreenPoint, TrackFailed};
use crate::kitty::unicode::{PlaceholderCell, PlaceholderCells};
use crate::plant::VtPlant;
use crate::screen::Screen;

/// A screen and its cell size, for kitty graphics.
pub struct KittyScreen<'s, 'a, P> {
    pub screen: &'s mut Screen<'a, P>,
    pub cell_px: Option<Pixels>,
}

/// The read-only half, for the placeholder scan (the renderer's).
pub struct KittyCells<'s, 'a, P> {
    pub screen: &'s Screen<'a, P>,
}

fn clamp_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn grid<P: Planted<VtPlant>>(s: &Screen<'_, P>, cell_px: Option<Pixels>) -> Grid {
    let cols = clamp_u16(s.cols());
    let rows = clamp_u16(s.rows());
    let px = cell_px.unwrap_or(Pixels { width: 0, height: 0 });
    Grid {
        cols,
        rows,
        width_px: px.width.saturating_mul(u32::from(cols)),
        height_px: px.height.saturating_mul(u32::from(rows)),
    }
}

impl<P: Planted<VtPlant>> GraphicsScreen for KittyScreen<'_, '_, P> {
    fn grid(&self) -> Grid {
        grid(self.screen, self.cell_px)
    }

    fn cursor(&self) -> ActivePoint {
        let c = self.screen.cursor;
        ActivePoint { row: clamp_u16(c.y), col: clamp_u16(c.x) }
    }

    fn track_cursor(&mut self) -> Result<PinId, TrackFailed> {
        let r = self.screen.cursor_r();
        let x = self.screen.cursor.x;
        self.screen.track_pin(r, x).map(PinId).map_err(|_| TrackFailed)
    }

    fn untrack(&mut self, pin: PinId) {
        self.screen.untrack_pin(pin.0);
    }

    fn locate(&self, pin: PinId) -> ScreenPoint {
        let (r, x) = self.screen.pin(pin.0).unwrap_or((0, 0));
        ScreenPoint { row: r as u64, col: clamp_u16(x) }
    }

    fn active(&self, at: ActivePoint) -> Option<ScreenPoint> {
        let (y, x) = (usize::from(at.row), usize::from(at.col));
        (y < self.screen.rows() && x < self.screen.cols())
            .then(|| ScreenPoint { row: self.screen.r(y) as u64, col: at.col })
    }

    fn last_row(&self) -> u64 {
        self.screen.pages.total().saturating_sub(1) as u64
    }
}

impl<P: Planted<VtPlant>> PlaceholderCells for KittyCells<'_, '_, P> {
    fn cols(&self) -> u16 {
        clamp_u16(self.screen.cols())
    }

    fn row_flagged(&self, row: u64) -> bool {
        usize::try_from(row)
            .ok()
            .filter(|&r| r < self.screen.pages.total())
            .is_some_and(|r| self.screen.pages.row(r).kitty_placeholder)
    }

    fn cell(&self, at: ScreenPoint) -> PlaceholderCell<'_> {
        let s = self.screen;
        let blank = PlaceholderCell {
            codepoint: ' ',
            fg: crate::style::Color::None,
            underline: crate::style::Color::None,
            marks: &[],
        };
        let Ok(r) = usize::try_from(at.row) else { return blank };
        let x = usize::from(at.col);
        if r >= s.pages.total() || x >= s.cols() {
            return blank;
        }
        let cell = s.pages.cell(r, x);
        let style = s.style(cell.style_id());
        PlaceholderCell {
            codepoint: char::from_u32(cell.codepoint()).unwrap_or(' '),
            fg: style.fg,
            underline: style.underline_color,
            marks: s.graphemes(cell),
        }
    }
}
