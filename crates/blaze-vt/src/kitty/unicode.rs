//! Virtual placements drawn by U+10EEEE placeholder cells (ghostty's graphics_unicode.zig).
//!
//! A program places an image with `U=1`, then prints U+10EEEE where it should show: the
//! foreground colour is the image id (its low 24 bits), the underline colour the placement id,
//! and up to three combining marks from [`DIACRITICS`] give the image row, column and the image
//! id's high 8 bits. A cell missing marks continues the run to its left. The cells are the
//! screen's, so they are read through [`PlaceholderCells`], which the screen implements.

use crate::style::Color;

use super::image::Image;
use super::num::round_u32;
use super::storage::{ImageStorage, Location, PlacementKey, ScreenPoint};

/// The placeholder code point.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// A cell as the placeholder scan reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlaceholderCell<'c> {
    pub codepoint: char,
    pub fg: Color,
    pub underline: Color,
    /// The code points after the first in the cell's grapheme (combining marks), as the
    /// screen keeps them.
    pub marks: &'c [u32],
}

/// The screen's cells, for the placeholder scan.
pub trait PlaceholderCells {
    /// Columns in a row.
    fn cols(&self) -> u16;
    /// Whether a row may hold a placeholder (ghostty's `kitty_virtual_placeholder` row flag, set
    /// when one is printed). Answering `true` for every row is correct, only slower.
    fn row_flagged(&self, row: u64) -> bool;
    /// The cell at `at`.
    fn cell(&self, at: ScreenPoint) -> PlaceholderCell<'_>;
}

/// A run of placeholder cells in one row naming one image.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VirtualPlacement {
    /// The run's first cell.
    pub at: ScreenPoint,
    pub image_id: u32,
    pub placement_id: u32,
    /// The image cell the run starts at, 0-based.
    pub col: u32,
    pub row: u32,
    /// The run's size in cells (always one row).
    pub width: u32,
    pub height: u32,
}

/// Why a virtual placement cannot be drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VirtualError {
    /// The placement's grid does not fit a cell count (`u16`), or the cell size is zero.
    GridOutOfBounds,
    /// No `U=1` placement of the image (with that id) is stored.
    MissingPlacement,
}

/// Where and what part of an image a renderer draws (ghostty's `RenderPlacement`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RenderPlacement {
    pub top_left: ScreenPoint,
    pub offset_x: u32,
    pub offset_y: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub dest_width: u32,
    pub dest_height: u32,
}

impl RenderPlacement {
    const fn nothing(top_left: ScreenPoint) -> RenderPlacement {
        RenderPlacement {
            top_left,
            offset_x: 0,
            offset_y: 0,
            source_x: 0,
            source_y: 0,
            source_width: 0,
            source_height: 0,
            dest_width: 0,
            dest_height: 0,
        }
    }
}

/// The placeholder runs from `from` (rightwards, then down), through row `last_row`
/// (`placementIterator`).
pub fn placements<C: PlaceholderCells>(
    cells: &C,
    from: ScreenPoint,
    last_row: u64,
) -> VirtualPlacements<'_, C> {
    VirtualPlacements { cells, row: from.row, col: from.col, last_row, done: false }
}

/// [`placements`]
pub struct VirtualPlacements<'c, C> {
    cells: &'c C,
    row: u64,
    col: u16,
    last_row: u64,
    done: bool,
}

impl<C: PlaceholderCells> VirtualPlacements<'_, C> {
    fn next_row(&mut self) {
        self.col = 0;
        match self.row.checked_add(1) {
            Some(r) if r <= self.last_row => self.row = r,
            _ => self.done = true,
        }
    }
}

impl<C: PlaceholderCells> Iterator for VirtualPlacements<'_, C> {
    type Item = VirtualPlacement;

    fn next(&mut self) -> Option<VirtualPlacement> {
        while !self.done && self.row <= self.last_row {
            if !self.cells.row_flagged(self.row) {
                self.next_row();
                continue;
            }
            let mut run: Option<Incomplete> = None;
            while self.col < self.cells.cols() {
                let at = ScreenPoint { row: self.row, col: self.col };
                let cell = self.cells.cell(at);
                if cell.codepoint != PLACEHOLDER {
                    if let Some(prev) = run {
                        // The next call starts at this cell, and skips it.
                        return Some(prev.complete());
                    }
                    self.col += 1;
                    continue;
                }
                let curr = Incomplete::new(at, &cell);
                match run.as_mut() {
                    Some(prev) => {
                        if !prev.append(&curr) {
                            // The next call starts a run at this cell.
                            return Some(prev.complete());
                        }
                    }
                    None => {
                        let mut prev = curr;
                        prev.row.get_or_insert(0);
                        prev.col.get_or_insert(0);
                        run = Some(prev);
                    }
                }
                self.col += 1;
            }
            self.next_row();
            if let Some(prev) = run {
                return Some(prev.complete());
            }
        }
        None
    }
}

/// One cell's placement fields (`IncompletePlacement`); missing marks continue a run.
#[derive(Clone, Copy, Debug)]
struct Incomplete {
    at: ScreenPoint,
    image_id_low: u32,
    image_id_high: Option<u8>,
    placement_id: Option<u32>,
    row: Option<u32>,
    col: Option<u32>,
    width: u32,
}

impl Incomplete {
    fn new(at: ScreenPoint, cell: &PlaceholderCell<'_>) -> Incomplete {
        let placement_id = match color_to_id(cell.underline) {
            0 => None,
            id => Some(id),
        };
        let mut r = Incomplete {
            at,
            image_id_low: color_to_id(cell.fg),
            image_id_high: None,
            placement_id,
            row: None,
            col: None,
            width: 1,
        };
        // An unknown mark is read as absent (ghostty logs it).
        let index = |m: u32| DIACRITICS.binary_search(&m).ok().map(|i| i as u32);
        if let Some(&m) = cell.marks.first() {
            r.row = index(m);
        }
        if let Some(&m) = cell.marks.get(1) {
            r.col = index(m);
        }
        if let Some(&m) = cell.marks.get(2) {
            r.image_id_high = index(m).and_then(|h| u8::try_from(h).ok());
        }
        r
    }

    fn append(&mut self, other: &Incomplete) -> bool {
        let col = self.col.expect("a run's start has a column");
        let fits = self.image_id_low == other.image_id_low
            && self.placement_id == other.placement_id
            && other.row.is_none_or(|r| Some(r) == self.row)
            && other.col.is_none_or(|c| c == col.wrapping_add(self.width))
            && other.image_id_high.is_none_or(|h| Some(h) == self.image_id_high);
        if fits {
            self.width += 1;
        }
        fits
    }

    fn complete(&self) -> VirtualPlacement {
        VirtualPlacement {
            at: self.at,
            image_id: self.image_id_low | (u32::from(self.image_id_high.unwrap_or(0)) << 24),
            placement_id: self.placement_id.unwrap_or(0),
            col: self.col.unwrap_or(0),
            row: self.row.unwrap_or(0),
            width: self.width,
            height: 1,
        }
    }
}

/// A colour as an id (`colorToId`): a palette index, or the RGB as 24 bits.
fn color_to_id(c: Color) -> u32 {
    match c {
        Color::None => 0,
        Color::Palette(v) => u32::from(v),
        Color::Rgb(rgb) => (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b),
    }
}

/// The row or column a mark stands for (`getIndex`), 0-based.
pub fn diacritic_index(cp: char) -> Option<u32> {
    DIACRITICS.binary_search(&(cp as u32)).ok().map(|i| i as u32)
}

impl VirtualPlacement {
    /// The rows and columns of the `U=1` placement this run draws part of (`grid`): its `r`
    /// and `c`, or the image's size in cells. With no placement id, the first virtual placement
    /// of the image in key order (ghostty takes the first in its hash map's order).
    fn grid(
        &self,
        storage: &ImageStorage<'_>,
        image: &Image<'_>,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<(u32, u32), VirtualError> {
        let placement = if self.placement_id > 0 {
            storage.placement(PlacementKey::external(self.image_id, self.placement_id))
        } else {
            storage
                .placements()
                .find(|(k, p)| k.image_id == self.image_id && p.location == Location::Virtual)
                .map(|(_, p)| p)
        };
        let placement = placement.ok_or(VirtualError::MissingPlacement)?;
        let mut rows = placement.rows;
        let mut columns = placement.columns;
        // ghostty divides by the cell size unchecked; a zero cell size is refused here.
        if (rows == 0 && cell_height == 0) || (columns == 0 && cell_width == 0) {
            return Err(VirtualError::GridOutOfBounds);
        }
        if rows == 0 {
            rows = image.height.div_ceil(cell_height);
        }
        if columns == 0 {
            columns = image.width.div_ceil(cell_width);
        }
        if rows > u32::from(u16::MAX) || columns > u32::from(u16::MAX) {
            return Err(VirtualError::GridOutOfBounds);
        }
        Ok((rows, columns))
    }

    /// What to draw for this run (`renderPlacement`): the image is fit in the placement's grid,
    /// centred, and this run's cells cut from it.
    pub fn render(
        &self,
        storage: &ImageStorage<'_>,
        img: &Image<'_>,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<RenderPlacement, VirtualError> {
        let (grid_rows, grid_cols) = self.grid(storage, img, cell_width, cell_height)?;
        let img_w = f64::from(img.width);
        let img_h = f64::from(img.height);
        let rows_px = f64::from(grid_rows) * f64::from(cell_height);
        let cols_px = f64::from(grid_cols) * f64::from(cell_width);

        // The scale to the placement's size and the centring offsets, in pixels.
        let (x_scale, y_scale, sx_offset, sy_offset) = if img_w * rows_px > img_h * cols_px {
            let s = cols_px / img_w.max(1.0);
            (s, s, 0.0, (rows_px - img_h * s) / 2.0)
        } else {
            let s = rows_px / img_h.max(1.0);
            (s, s, (cols_px - img_w * s) / 2.0, 0.0)
        };

        // The image padded to the placement's aspect, in image pixels.
        let pad_x = sx_offset / x_scale;
        let pad_y = sy_offset / y_scale;
        let scaled_w = img_w + pad_x * 2.0;
        let scaled_h = img_h + pad_y * 2.0;

        // This run's part of the padded image.
        let grid_cols_f = f64::from(grid_cols);
        let grid_rows_f = f64::from(grid_rows);
        let mut src_w = scaled_w * (f64::from(self.width) / grid_cols_f);
        let mut src_h = scaled_h * (f64::from(self.height) / grid_rows_f);
        let mut src_x = scaled_w * (f64::from(self.col) / grid_cols_f);
        let mut src_y = scaled_h * (f64::from(self.row) / grid_rows_f);

        // Cut the padding from the source and the destination.
        let mut x_offset = 0.0;
        let mut y_offset = 0.0;
        let mut width = f64::from(self.width) * f64::from(cell_width);
        let mut height = f64::from(self.height) * f64::from(cell_height);
        if src_y < pad_y {
            let offset = pad_y - src_y;
            src_h -= offset;
            y_offset = offset;
            height -= offset * y_scale;
            src_y = 0.0;
            if src_h > img_h {
                src_h = img_h;
                height = img_h * y_scale;
            }
        } else if src_y + src_h > scaled_h - pad_y {
            src_y -= pad_y;
            src_h = scaled_h - pad_y - src_y;
            src_h -= pad_y;
            height = src_h * y_scale;
        } else {
            src_y -= pad_y;
        }
        if src_x < pad_x {
            let offset = pad_x - src_x;
            src_w -= offset;
            x_offset = offset;
            width -= offset * x_scale;
            src_x = 0.0;
            if src_w > img_w {
                src_w = img_w;
                width = img_w * x_scale;
            }
        } else if src_x + src_w > scaled_w - pad_x {
            src_x -= pad_x;
            src_w = scaled_w - pad_x - src_x;
            src_w -= pad_x;
            width = src_w * x_scale;
        } else {
            src_x -= pad_x;
        }
        // The run is all padding: nothing to draw.
        if src_w <= 0.0 || src_h <= 0.0 {
            return Ok(RenderPlacement::nothing(self.at));
        }
        Ok(RenderPlacement {
            top_left: self.at,
            offset_x: round_u32(x_offset * x_scale),
            offset_y: round_u32(y_offset * y_scale),
            source_x: round_u32(src_x),
            source_y: round_u32(src_y),
            source_width: round_u32(src_w),
            source_height: round_u32(src_h),
            dest_width: round_u32(width),
            dest_height: round_u32(height),
        })
    }
}

/// The marks that number rows and columns: a mark's index is its value. From kitty's
/// rowcolumn-diacritics.txt, as ghostty has it; sorted, for the binary search.
pub const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::image::Media;
    use crate::kitty::storage::tests::{FakeScreen, alloc, image};
    use crate::kitty::storage::{Generations, Placement, StorageLimit};
    use std::vec::Vec;

    /// A grid of cells written as strings: each string is one cell's grapheme.
    struct Cells {
        cols: u16,
        rows: Vec<Vec<(Vec<u32>, Color, Color)>>,
    }

    impl Cells {
        fn new(cols: u16, rows: usize) -> Cells {
            Cells { cols, rows: (0..rows).map(|_| Vec::new()).collect() }
        }

        /// Print `text` on `row` from the left, splitting graphemes at each non-mark.
        fn print(&mut self, row: usize, text: &str, fg: Color, underline: Color) {
            for c in text.chars() {
                let is_mark = DIACRITICS.binary_search(&(c as u32)).is_ok();
                let r = &mut self.rows[row];
                match r.last_mut() {
                    Some(cell) if is_mark => cell.0.push(c as u32),
                    _ => r.push((std::vec![c as u32], fg, underline)),
                }
            }
        }
    }

    impl PlaceholderCells for Cells {
        fn cols(&self) -> u16 {
            self.cols
        }
        fn row_flagged(&self, row: u64) -> bool {
            self.rows[row as usize].iter().any(|c| c.0[0] == PLACEHOLDER as u32)
        }
        fn cell(&self, at: ScreenPoint) -> PlaceholderCell<'_> {
            match self.rows[at.row as usize].get(usize::from(at.col)) {
                Some((cps, fg, ul)) => PlaceholderCell {
                    codepoint: char::from_u32(cps[0]).unwrap_or(' '),
                    fg: *fg,
                    underline: *ul,
                    marks: &cps[1..],
                },
                None => PlaceholderCell {
                    codepoint: ' ',
                    fg: Color::None,
                    underline: Color::None,
                    marks: &[],
                },
            }
        }
    }

    fn all(cells: &Cells) -> Vec<VirtualPlacement> {
        placements(cells, ScreenPoint { row: 0, col: 0 }, cells.rows.len() as u64 - 1).collect()
    }

    fn one(cells: &Cells) -> VirtualPlacement {
        let v = all(cells);
        assert_eq!(v.len(), 1, "{v:?}");
        v[0]
    }

    fn fields(p: VirtualPlacement) -> (u32, u32, u32, u32, u32) {
        (p.image_id, p.placement_id, p.row, p.col, p.width)
    }

    const N: Color = Color::None;

    // ghostty graphics_unicode.zig:870 "unicode diacritic sorted"
    #[test]
    fn unicode_diacritic_sorted() {
        assert!(DIACRITICS.is_sorted());
    }

    // ghostty graphics_unicode.zig:880 "unicode diacritic"
    #[test]
    fn unicode_diacritic() {
        assert_eq!(diacritic_index('\u{483}'), Some(30));
        assert_eq!(diacritic_index('\u{1d242}'), Some(294));
        assert_eq!(diacritic_index('a'), None);
    }

    // ghostty graphics_unicode.zig:886 "unicode placement: none"
    #[test]
    fn unicode_placement_none() {
        let mut c = Cells::new(5, 5);
        for (row, s) in ["hello", "world", "1", "2"].iter().enumerate() {
            c.print(row, s, N, N);
        }
        assert!(all(&c).is_empty());
    }

    // ghostty graphics_unicode.zig:901 "unicode placement: single row/col"
    #[test]
    fn unicode_placement_single_row_col() {
        let mut c = Cells::new(5, 5);
        c.print(0, "\u{10EEEE}\u{0305}\u{0305}", N, N);
        assert_eq!(fields(one(&c)), (0, 0, 0, 0, 1));
    }

    // ghostty graphics_unicode.zig:925 "unicode placement: continuation break"
    #[test]
    fn unicode_placement_continuation_break() {
        let mut c = Cells::new(10, 5);
        c.print(0, "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030E}", N, N);
        let v = all(&c);
        assert_eq!(
            v.iter().map(|p| fields(*p)).collect::<Vec<_>>(),
            [(0, 0, 0, 0, 1), (0, 0, 0, 2, 1)]
        );
    }

    // ghostty graphics_unicode.zig:959 "unicode placement: continuation with diacritics set"
    #[test]
    fn unicode_placement_continuation_with_diacritics_set() {
        let mut c = Cells::new(10, 5);
        c.print(
            0,
            "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\u{10EEEE}\u{0305}\u{030E}",
            N,
            N,
        );
        assert_eq!(fields(one(&c)), (0, 0, 0, 0, 3));
    }

    // ghostty graphics_unicode.zig:986 "unicode placement: continuation with no col"
    #[test]
    fn unicode_placement_continuation_with_no_col() {
        let mut c = Cells::new(10, 5);
        c.print(0, "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{10EEEE}\u{0305}", N, N);
        assert_eq!(fields(one(&c)), (0, 0, 0, 0, 3));
    }

    // ghostty graphics_unicode.zig:1013 "unicode placement: continuation with no diacritics"
    #[test]
    fn unicode_placement_continuation_with_no_diacritics() {
        let mut c = Cells::new(10, 5);
        c.print(0, "\u{10EEEE}\u{10EEEE}\u{10EEEE}", N, N);
        assert_eq!(fields(one(&c)), (0, 0, 0, 0, 3));
    }

    // ghostty graphics_unicode.zig:1040 "unicode placement: run ending"
    #[test]
    fn unicode_placement_run_ending() {
        let mut c = Cells::new(10, 5);
        c.print(0, "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}ABC", N, N);
        assert_eq!(fields(one(&c)), (0, 0, 0, 0, 2));
    }

    // ghostty graphics_unicode.zig:1067 "unicode placement: run starting in the middle"
    #[test]
    fn unicode_placement_run_starting_in_the_middle() {
        let mut c = Cells::new(10, 5);
        c.print(0, "ABC\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}", N, N);
        let p = one(&c);
        assert_eq!(fields(p), (0, 0, 0, 0, 2));
        assert_eq!(p.at, ScreenPoint { row: 0, col: 3 });
    }

    // ghostty graphics_unicode.zig:1094 "unicode placement: specifying image id as palette"
    #[test]
    fn unicode_placement_specifying_image_id_as_palette() {
        let mut c = Cells::new(5, 5);
        c.print(0, "\u{10EEEE}\u{0305}\u{0305}", Color::Palette(42), N);
        assert_eq!(fields(one(&c)), (42, 0, 0, 0, 1));
    }

    // ghostty graphics_unicode.zig:1119 "unicode placement: specifying image id with high bits"
    #[test]
    fn unicode_placement_specifying_image_id_with_high_bits() {
        let mut c = Cells::new(5, 5);
        c.print(0, "\u{10EEEE}\u{0305}\u{0305}\u{030E}", Color::Palette(42), N);
        assert_eq!(fields(one(&c)), (33554474, 0, 0, 0, 1));
    }

    // ghostty graphics_unicode.zig:1144 "unicode placement: specifying placement id as palette"
    #[test]
    fn unicode_placement_specifying_placement_id_as_palette() {
        let mut c = Cells::new(5, 5);
        c.print(0, "\u{10EEEE}\u{0305}\u{0305}", Color::Palette(42), Color::Palette(21));
        assert_eq!(fields(one(&c)), (42, 21, 0, 0, 1));
    }

    #[test]
    fn unicode_placement_rgb_ids_and_rows() {
        let mut c = Cells::new(4, 3);
        let fg = Color::Rgb(crate::style::Rgb { r: 1, g: 2, b: 3 });
        c.print(1, "\u{10EEEE}\u{030D}\u{10EEEE}", fg, N);
        c.print(2, "\u{10EEEE}\u{030E}\u{10EEEE}\u{030D}", fg, N);
        let v = all(&c);
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].image_id, v[0].row, v[0].width, v[0].at.row), (0x010203, 1, 2, 1));
        // A different row mark breaks the run.
        assert_eq!((v[1].row, v[1].width), (2, 1));
        assert_eq!((v[2].row, v[2].col, v[2].at.col), (1, 0, 1));
        // A scan from the middle of a row, through one row only.
        let v: Vec<_> = placements(&c, ScreenPoint { row: 1, col: 1 }, 1).collect();
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].at.col, v[0].width), (1, 1));
    }

    fn dog(columns: u32, rows: u32) -> ImageStorage<'static> {
        let mut t = FakeScreen::new(5, 5);
        let mut g = Generations::default();
        let mut s = ImageStorage::new_in(StorageLimit::DEFAULT, Media::DIRECT, alloc());
        s.add_image(image(1, 500, 306), &mut t, &mut g).unwrap();
        let p = Placement { columns, rows, ..Placement::at(Location::Virtual) };
        s.add_placement(1, 0, p, &mut g).unwrap();
        s
    }

    fn run(row: u32) -> VirtualPlacement {
        VirtualPlacement {
            at: ScreenPoint { row: 0, col: 0 },
            image_id: 1,
            placement_id: 0,
            col: 0,
            row,
            width: 4,
            height: 1,
        }
    }

    fn rendered(r: RenderPlacement) -> [u32; 8] {
        [
            r.offset_x,
            r.offset_y,
            r.source_x,
            r.source_y,
            r.source_width,
            r.source_height,
            r.dest_width,
            r.dest_height,
        ]
    }

    // ghostty graphics_unicode.zig:1175 "unicode render placement: dog 4x2"
    #[test]
    fn unicode_render_placement_dog_4x2() {
        let s = dog(4, 2);
        let img = s.image_by_id(1).unwrap();
        assert_eq!(
            rendered(run(0).render(&s, img, 36, 80).unwrap()),
            [0, 36, 0, 0, 500, 153, 144, 44]
        );
        assert_eq!(
            rendered(run(1).render(&s, img, 36, 80).unwrap()),
            [0, 0, 0, 153, 500, 153, 144, 44]
        );
    }

    // ghostty graphics_unicode.zig:1242 "unicode render placement: dog 2x2 with blank cells"
    #[test]
    fn unicode_render_placement_dog_2x2_with_blank_cells() {
        let s = dog(2, 2);
        let img = s.image_by_id(1).unwrap();
        assert_eq!(
            rendered(run(0).render(&s, img, 36, 80).unwrap()),
            [0, 58, 0, 0, 500, 153, 72, 22]
        );
        assert_eq!(
            rendered(run(1).render(&s, img, 36, 80).unwrap()),
            [0, 0, 0, 153, 500, 153, 72, 22]
        );
    }

    // ghostty graphics_unicode.zig:1308 "unicode render placement: dog 1x1"
    #[test]
    fn unicode_render_placement_dog_1x1() {
        let s = dog(1, 1);
        let img = s.image_by_id(1).unwrap();
        assert_eq!(
            rendered(run(0).render(&s, img, 36, 80).unwrap()),
            [0, 29, 0, 0, 500, 306, 36, 22]
        );
    }

    #[test]
    fn render_placement_errors() {
        let s = dog(0, 0);
        let img = s.image_by_id(1).unwrap();
        // The grid from the image: 500/36 and 306/80 rounded up.
        assert!(run(0).render(&s, img, 36, 80).is_ok());
        assert_eq!(run(0).render(&s, img, 0, 80), Err(VirtualError::GridOutOfBounds));
        let missing = VirtualPlacement { placement_id: 7, ..run(0) };
        assert_eq!(missing.render(&s, img, 36, 80), Err(VirtualError::MissingPlacement));
        let s = dog(70_000, 1);
        let img = s.image_by_id(1).unwrap();
        assert_eq!(run(0).render(&s, img, 36, 80), Err(VirtualError::GridOutOfBounds));
    }
}
