//! What one frame draws, as plain data: the view flattens the world into a [`Draw`] and gpu.rs draws it. Nothing here touches
//! a GPU. Copied from den-native's draw.rs (github.com/andrewgazelka/den) and stripped to panels, glyphs and lines.
use std::rc::Rc;

use crate::media;
use crate::pack::{Frame, GlyphInst, ImageInst, LineInst, PanelInst, PathInst, SpriteInst, SurfaceInst};

pub const ATLAS_SIZE: u32 = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegKind {
    Panel,
    Glyph,
    /// Indices into [`Draw::lines`]: stroked segments and dots, anti-aliased, blended in draw order with the rest.
    Line,
    /// Indices into [`Draw::sprites`]: procedural cells (box drawing, blocks, braille, sextants, powerline).
    Sprite,
    /// Indices into [`Draw::paths`]: filled polygons over [`Draw::edges`], anti-aliased, blended in draw order.
    Path,
    /// One index into [`Draw::images`] (each binds its own texture, so a run is never more than one).
    Image,
    /// Indices into [`Draw::kips`]: Kip (harness-native-kip), each its own picture rendered before the frame and set in
    /// over its contact shadow (kip.rs).
    Kip,
    /// One index into [`Draw::surfaces`]: a live picture from the host's Media, bound per draw like an image's.
    Surface,
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
    pub paths: Vec<PathInst>,
    /// Every path's edges (ax, ay, bx, by in points), each path a run of them.
    pub edges: Vec<[f32; 4]>,
    /// Pictures and their texels (`size` wide and high), one texture each: straight sRGB RGBA8, or for an HDR picture
    /// (`ImageInst::hdr[0]` = 1) premultiplied linear RGBA half floats, 8 bytes a texel.
    pub images: Vec<ImageInst>,
    pub texels: Vec<Rc<[u8]>>,
    /// The Kips this frame draws (kip.rs renders each picture before the frame, the pass sets them in by `segs`).
    pub kips: Vec<harness_native_kip::MascotQuad>,
    /// Live pictures and the Media key each shows (`surface_keys[i]` for `surfaces[i]`).
    pub surfaces: Vec<SurfaceInst>,
    pub surface_keys: Vec<u64>,
    /// What this frame asks of Media (shaders to render, videos to step): plain data, filled by the host after it paints;
    /// `Renderer::render` hands it to `Media::prepare` before it encodes.
    pub media: Vec<media::Spec>,
    /// Paint order: runs of instances by kind.
    pub segs: Vec<Seg>,
}

impl Draw {
    fn push_seg(&mut self, kind: SegKind, index: usize) {
        if let Some(last) = self.segs.last_mut()
            && last.kind == kind
            && kind != SegKind::Image
            && kind != SegKind::Surface
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

    pub fn path(&mut self, p: PathInst) {
        self.paths.push(p);
        self.push_seg(SegKind::Path, self.paths.len() - 1);
    }

    pub fn sprite(&mut self, s: SpriteInst) {
        self.sprites.push(s);
        self.push_seg(SegKind::Sprite, self.sprites.len() - 1);
    }

    /// A Kip: its picture and its shadow, in paint order.
    pub fn kip(&mut self, q: harness_native_kip::MascotQuad) {
        self.kips.push(q);
        self.push_seg(SegKind::Kip, self.kips.len() - 1);
    }

    /// A picture: `texels` (`size[0]` by `size[1]`: straight RGBA8, or RGBA half floats for an HDR one) over `img.rect`.
    pub fn image(&mut self, img: ImageInst, texels: Rc<[u8]>) {
        debug_assert_eq!(texels.len(), img.size[0] as usize * img.size[1] as usize * img.texel_bytes());
        self.images.push(img);
        self.texels.push(texels);
        self.push_seg(SegKind::Image, self.images.len() - 1);
    }

    /// A live picture: Media's latest frame for `key` over `inst.rect` (nothing is drawn while Media has none).
    pub fn surface(&mut self, inst: SurfaceInst, key: u64) {
        self.surfaces.push(inst);
        self.surface_keys.push(key);
        self.push_seg(SegKind::Surface, self.surfaces.len() - 1);
    }
}
