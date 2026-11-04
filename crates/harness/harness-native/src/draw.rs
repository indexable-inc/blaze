//! What one frame draws, as plain data: the view flattens the world into a [`Draw`] and gpu.rs draws it. Nothing here touches
//! a GPU. Copied from den-native's draw.rs (github.com/andrewgazelka/den) and stripped to panels, glyphs and lines.
use crate::pack::{Frame, GlyphInst, LineInst, PanelInst, SpriteInst};

pub const ATLAS_SIZE: u32 = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegKind {
    Panel,
    Glyph,
    /// Indices into [`Draw::lines`]: stroked segments and dots, anti-aliased, blended in draw order with the rest.
    Line,
    /// Indices into [`Draw::sprites`]: procedural cells (box drawing, blocks, braille, sextants, powerline).
    Sprite,
}

/// A run of consecutive instances of one kind, drawn in order.
#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub kind: SegKind,
    pub first: u32,
    pub count: u32,
}

/// Everything one frame draws.
#[derive(Default)]
pub struct Draw {
    pub frame: Frame,
    pub clear: [f32; 4],
    pub panels: Vec<PanelInst>,
    pub glyphs: Vec<GlyphInst>,
    pub lines: Vec<LineInst>,
    pub sprites: Vec<SpriteInst>,
    /// Paint order: runs of instances by kind.
    pub segs: Vec<Seg>,
}

impl Draw {
    fn push_seg(&mut self, kind: SegKind, index: usize) {
        if let Some(last) = self.segs.last_mut()
            && last.kind == kind
            && (last.first + last.count) as usize == index
        {
            last.count += 1;
            return;
        }
        self.segs.push(Seg { kind, first: index as u32, count: 1 });
    }

    pub fn panel(&mut self, p: PanelInst) {
        self.panels.push(p);
        self.push_seg(SegKind::Panel, self.panels.len() - 1);
    }

    pub fn glyph(&mut self, g: GlyphInst) {
        self.glyphs.push(g);
        self.push_seg(SegKind::Glyph, self.glyphs.len() - 1);
    }

    pub fn line(&mut self, l: LineInst) {
        self.lines.push(l);
        self.push_seg(SegKind::Line, self.lines.len() - 1);
    }

    pub fn sprite(&mut self, s: SpriteInst) {
        self.sprites.push(s);
        self.push_seg(SegKind::Sprite, self.sprites.len() - 1);
    }
}
