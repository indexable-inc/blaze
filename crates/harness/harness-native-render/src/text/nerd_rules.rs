//! Glyph constraints: how a glyph from another font (a Nerd Font icon, an emoji, a fallback symbol) is scaled and moved
//! to sit in the terminal's cells. A port of Ghostty's `Glyph.RenderOptions.Constraint` (src/font/Glyph.zig, MIT), with
//! its Nerd Font table ([`super::nerd`]) as data.
//!
//! Sizes are device pixels; `y` is up from the cell's bottom edge, `x` right from its left edge.

/// Sizing rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    None,
    /// Scale down to fit, keeping the aspect ratio.
    Fit,
    /// Scale up or down to fill the bounds, keeping the aspect ratio.
    Cover,
    /// Fit, but scale up to cover at least one cell (Nerd Font rule).
    FitCover1,
    /// Fill the bounds exactly both ways.
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    Start,
    End,
    Center,
    /// Centred on the first cell even when the constraint spans two (Nerd Font rule).
    Center1,
}

/// What a glyph is sized against vertically when its constraint is one cell wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Height {
    Cell,
    Icon,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Constraint {
    pub size: Size,
    pub height: Height,
    /// At most this many cells wide.
    pub max_w: u8,
    pub align_x: Align,
    pub align_y: Align,
    pub pad_top: f64,
    pub pad_left: f64,
    pub pad_right: f64,
    pub pad_bottom: f64,
    /// The glyph's box within its scale group's (Nerd Font icons sized as a set).
    pub rel_w: f64,
    pub rel_h: f64,
    pub rel_x: f64,
    pub rel_y: f64,
    /// Widest aspect ratio a stretch may make, 0 for none.
    pub max_xy: f64,
}

impl Constraint {
    pub const NONE: Self = Self {
        size: Size::None,
        height: Height::Cell,
        max_w: 2,
        align_x: Align::None,
        align_y: Align::None,
        pad_top: 0.0,
        pad_left: 0.0,
        pad_right: 0.0,
        pad_bottom: 0.0,
        rel_w: 1.0,
        rel_h: 1.0,
        rel_x: 0.0,
        rel_y: 0.0,
        max_xy: 0.0,
    };
    /// A symbol from a fallback font: shrunk to fit its cells (Ghostty's `Renderer.addGlyph`).
    pub const FIT: Self = Self { size: Size::Fit, ..Self::NONE };
    /// An emoji: fills its cells, centred, with a sliver of room each side (Ghostty's `SharedGrid.renderGlyph`).
    pub const EMOJI: Self = Self {
        size: Size::Cover,
        align_x: Align::Center,
        align_y: Align::Center,
        pad_left: 0.025,
        pad_right: 0.025,
        ..Self::NONE
    };

    fn does_anything(&self) -> bool {
        self.size != Size::None || self.align_x != Align::None || self.align_y != Align::None
    }

    /// `glyph` moved and scaled into cells of `m`, given `width` cells free for it.
    #[must_use]
    pub fn constrain(&self, glyph: Rect, m: &Metrics, width: u8) -> Rect {
        if !self.does_anything() {
            return glyph;
        }
        if self.size == Size::Stretch {
            // Stretched glyphs meet across cells: scale them to the grid, not the face, and with no negative padding.
            let m = Metrics { face_w: m.cell_w, face_h: m.cell_h, face_y: 0.0, ..*m };
            let c = Self {
                pad_bottom: self.pad_bottom.max(0.0),
                pad_top: self.pad_top.max(0.0),
                pad_left: self.pad_left.max(0.0),
                pad_right: self.pad_right.max(0.0),
                ..*self
            };
            return c.inner(glyph, &m, width);
        }
        self.inner(glyph, m, width)
    }

    fn inner(&self, glyph: Rect, m: &Metrics, width: u8) -> Rect {
        let min_w = if self.size == Size::Stretch && m.face_w > 0.9 * m.face_h {
            1
        } else {
            self.max_w.min(width)
        };
        let gw = glyph.w / self.rel_w;
        let gh = glyph.h / self.rel_h;
        let mut group =
            Rect { w: gw, h: gh, x: glyph.x - gw * self.rel_x, y: glyph.y - gh * self.rel_y };
        let (fx, fy) = self.factors(group, m, min_w);
        let (cx, cy) = (group.x + group.w / 2.0, group.y + group.h / 2.0);
        group.w *= fx;
        group.h *= fy;
        group.x = cx - group.w / 2.0;
        group.y = cy - group.h / 2.0;
        group.y = self.aligned_y(group, m);
        group.x = self.aligned_x(group, m, min_w);
        Rect {
            w: fx * glyph.w,
            h: fy * glyph.h,
            x: group.x + group.w * self.rel_x,
            y: group.y + group.h * self.rel_y,
        }
    }

    fn factors(&self, group: Rect, m: &Metrics, min_w: u8) -> (f64, f64) {
        if self.size == Size::None {
            return (1.0, 1.0);
        }
        let multi = min_w > 1;
        let tw = (f64::from(min_w) - (self.pad_left + self.pad_right)) * m.face_w;
        let th = (1.0 - (self.pad_bottom + self.pad_top))
            * match self.height {
                Height::Cell => m.face_h,
                Height::Icon if multi => m.icon_h,
                Height::Icon => m.icon_h1,
            };
        let (mut fx, mut fy) = (tw / group.w, th / group.h);
        match self.size {
            Size::None | Size::Stretch => {}
            Size::Fit => {
                fy = 1.0_f64.min(fx).min(fy);
                fx = fy;
            }
            Size::Cover => {
                fy = fx.min(fy);
                fx = fy;
            }
            Size::FitCover1 => {
                fy = fx.min(fy);
                if multi && fy > 1.0 {
                    fy = self.factors(group, m, 1).1.max(1.0);
                }
                fx = fy;
            }
        }
        if self.max_xy > 0.0 && group.w * fx > group.h * fy * self.max_xy {
            fx = group.h * fy * self.max_xy / group.w;
        }
        (fx, fy)
    }

    fn aligned_y(&self, group: Rect, m: &Metrics) -> f64 {
        if self.size == Size::None && self.align_y == Align::None {
            return group.y;
        }
        let start = m.face_y + self.pad_bottom * m.face_h;
        let end = m.face_y + (m.face_h - group.h - self.pad_top * m.face_h);
        let center = (start + end) / 2.0;
        match self.align_y {
            Align::None => {
                if end < start {
                    center
                } else {
                    start.max(group.y.min(end))
                }
            }
            Align::Start => start,
            Align::End => end,
            Align::Center | Align::Center1 => center,
        }
    }

    fn aligned_x(&self, group: Rect, m: &Metrics, min_w: u8) -> f64 {
        if self.size == Size::None && self.align_x == Align::None {
            return group.x;
        }
        let span = m.face_w + f64::from(min_w - 1) * m.cell_w;
        let start = self.pad_left * m.face_w;
        let end = span - group.w - self.pad_right * m.face_w;
        match self.align_x {
            Align::None => start.max(group.x.min(end)),
            Align::Start => start,
            Align::End => start.max(end),
            Align::Center => start.max((start + end) / 2.0),
            Align::Center1 => {
                start.max((start + (m.face_w - group.w - self.pad_right * m.face_w)) / 2.0)
            }
        }
    }
}

/// A glyph's ink box: size and bottom-left corner, device pixels from the cell's bottom-left (y up).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub w: f64,
    pub h: f64,
    pub x: f64,
    pub y: f64,
}

/// The grid's metrics as constraints read them (Ghostty's `Metrics`), device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub cell_w: f64,
    pub cell_h: f64,
    /// The primary face's unrounded advance and line height.
    pub face_w: f64,
    pub face_h: f64,
    /// The face's bottom above the cell's bottom (the baseline is snapped to a pixel; the face is not).
    pub face_y: f64,
    /// Icon heights for two-cell and one-cell constraints (font-patcher's heuristic).
    pub icon_h: f64,
    pub icon_h1: f64,
}

/// Ghostty's Nerd Font constraint for `cp`, if it has one.
#[must_use]
pub fn nerd(cp: u32) -> Option<Constraint> {
    let table = &super::nerd::RANGES;
    let i = table.partition_point(|r| r.1 < cp);
    let &(a, _, k) = table.get(i)?;
    (a <= cp).then(|| super::nerd::CONSTRAINTS[usize::from(k) - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ghostty's own expectations (Glyph.zig "Constraints" test: JetBrains Mono at 12 pt, 96 DPI).
    const M: Metrics = Metrics {
        cell_w: 10.0,
        cell_h: 22.0,
        face_w: 9.6,
        face_h: 21.12,
        face_y: 0.2,
        icon_h: 21.12,
        icon_h1: 44.48 / 3.0,
    };

    fn close(a: Rect, b: Rect) {
        for (x, y) in [(a.w, b.w), (a.h, b.h), (a.x, b.x), (a.y, b.y)] {
            assert!((x - y).abs() < 1e-6, "{a:?} != {b:?}");
        }
    }

    #[test]
    fn nerd_default_and_stretch() {
        let c = nerd(0xEA61).unwrap();
        assert_eq!(
            (c.size, c.height, c.align_x, c.align_y),
            (Size::FitCover1, Height::Icon, Align::Center1, Align::Center1)
        );
        let g = Rect { w: 9.015625, h: 13.015625, x: 3.015625, y: 3.76525 };
        close(
            c.constrain(g, &M, 1),
            Rect { w: 7.2125, h: 10.4125, x: 0.8125, y: 5.950695224719102 },
        );
        close(c.constrain(g, &M, 2), Rect { w: g.w, h: g.h, x: 1.015625, y: 4.7483690308988775 });
        let s = nerd(0xE0C0).unwrap();
        let g = Rect { w: 16.796875, h: 16.46875, x: -0.796875, y: 1.7109375 };
        close(s.constrain(g, &M, 1), Rect { w: 10.0, h: 22.0, x: 0.0, y: 0.0 });
        close(s.constrain(g, &M, 2), Rect { w: 20.0, h: 22.0, x: 0.0, y: 0.0 });
        assert!(nerd(u32::from('A')).is_none());
    }

    #[test]
    fn emoji_and_symbol() {
        close(
            Constraint::EMOJI.constrain(Rect { w: 20.0, h: 20.0, x: 0.46, y: 1.0 }, &M, 2),
            Rect { w: 18.72, h: 18.72, x: 0.44, y: 1.4 },
        );
        let sq = Rect { w: 10.272, h: 10.272, x: 2.864, y: 5.304 };
        close(Constraint::FIT.constrain(sq, &M, 1), Rect { w: 9.6, h: 9.6, x: 0.0, y: 5.64 });
        close(Constraint::FIT.constrain(sq, &M, 2), sq);
    }
}
