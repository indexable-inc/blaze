//! den's typesetter: shapes, breaks and sets den's text, and finds its carets and hits, from HarfRust (OpenType and AAT
//! shaping, `trak`), ICU4X (line and grapheme breaks, scripts, emoji presentation) and unicode-bidi (UAX #9). What it
//! asks of the platform is [`FontSource`] (text/font.rs; CoreText on macOS, fontdb and skrifa on Linux): a font's data,
//! at the variation the system sets it at (its optical size the point size unless something else is asked for), each
//! glyph's advance, and the font a character a font lacks falls back to (fed1eb428d:docs/research/text-shaping.md).
//!
//! A text is shaped whole to measure where its lines break, and each line is shaped again on its own.
use super::font::{Font as SystemFont, FontSource};
use super::{Attrs, FontId, Glyph, Read, breaks, hangs};
use harfrust::{
    Buffer, BufferFlags, ClusterLevel, Direction, Feature, Script, ShapeOptions, ShapePlan,
    ShaperFont, Tag,
};
use icu_properties::props::{EmojiPresentation, LineBreak, Script as Uscript};
use icu_properties::{CodePointMapData, CodePointSetData, PropertyNamesShort};
use icu_segmenter::options::LineBreakOptions;
use icu_segmenter::{GraphemeClusterSegmenter, LineSegmenter};
use read_fonts::model::{Font, Variation};
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use unicode_bidi::{BidiClass, Level};

/// Sub-units of a font unit positions are shaped in: HarfRust rounds each position to a whole unit of its scale, and a
/// varied font's advances fall between font units.
const SUB: i32 = 64;

/// A face at its variation coordinates (each axis tag and value, as bits).
type InstanceKey = (FontId, Vec<(u32, u32)>);

/// What the typesetter has read of the fonts: each font's tables by PostScript name, each (font, size, features) as
/// shaped, and the font the system falls back to for a cluster a font lacks.
#[derive(Default)]
pub(super) struct Fonts {
    faces: HashMap<String, Option<Font>>,
    instances: HashMap<InstanceKey, Option<Rc<Instance>>>,
    fallback: HashMap<(FontId, Vec<u16>), SystemFont>,
}

/// A font at one size and variation, with the features it is set with.
struct Instance {
    /// The system font it stands for, which its glyphs are rasterised from.
    id: FontId,
    font: Font,
    size: f32,
    /// Points per shaped unit.
    scale: f64,
    /// The font's own features (tabular figures), and the same with kerning off.
    features: [Vec<Feature>; 2],
    plans: RefCell<HashMap<(Tag, bool, bool), Rc<ShapePlan>>>,
    /// What the font's `trak` table adds to a glyph at this size as HarfRust reads it, shaped units.
    trak: OnceCell<i32>,
    /// The system font, and each glyph's advance as it has it, points.
    system: SystemFont,
    advances: RefCell<HashMap<u16, f64>>,
}

/// `font`'s data, read through the platform on first use.
fn face_of(font: &SystemFont) -> Option<Font> {
    let (source, index) = font.source()?;
    Font::new(source, index)
}

/// The variation `font` is set at: the axes the system reports away from their defaults, and its optical size the
/// point size unless one is reported (the system sizes a font with an `opsz` axis to the size it is set at).
fn variations_of(font: &SystemFont, size: f32) -> Vec<Variation> {
    let mut out: Vec<Variation> = font
        .variations()
        .into_iter()
        .map(|(k, v)| Variation::new(Tag::from_be_bytes(k.to_be_bytes()), v))
        .collect();
    if !out.iter().any(|v| v.tag == Tag::new(b"opsz")) {
        out.push(Variation::new(Tag::new(b"opsz"), size));
    }
    out
}

/// The instance `system` is shaped as, read on first use; none when its tables cannot be read.
fn instance(fonts: &mut super::Fonts, system: &SystemFont) -> Option<Rc<Instance>> {
    let id = fonts.register(system);
    let mut feats = system.features();
    if fonts.look.plain {
        feats.extend([(u32::from_be_bytes(*b"liga"), 0), (u32::from_be_bytes(*b"calt"), 0)]);
    }
    let key = (id, feats);
    if let Some(i) = fonts.shaper.instances.get(&key) {
        return i.clone();
    }
    let size = system.size();
    let face = fonts
        .shaper
        .faces
        .entry(system.post_script_name())
        .or_insert_with(|| face_of(system))
        .clone();
    let made = face.map(|face| {
        let font = face.instance_builder().variations(variations_of(system, size)).build();
        let upem = i32::from(font.units_per_em());
        let own: Vec<Feature> = key
            .1
            .iter()
            .map(|&(t, v)| Feature::new(Tag::from_be_bytes(t.to_be_bytes()), v, ..))
            .collect();
        let mut unkerned = own.clone();
        unkerned.push(Feature::new(Tag::new(b"kern"), 0, ..));
        Rc::new(Instance {
            id,
            font,
            size,
            scale: f64::from(size) / f64::from(upem * SUB),
            features: [own, unkerned],
            plans: RefCell::new(HashMap::new()),
            trak: OnceCell::new(),
            system: system.clone(),
            advances: RefCell::new(HashMap::new()),
        })
    });
    fonts.shaper.instances.insert(key, made.clone());
    made
}

impl Instance {
    fn plan(&self, script: Script, rtl: bool, kerning: bool) -> Rc<ShapePlan> {
        let key = (script.tag(), rtl, kerning);
        self.plans
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| {
                let dir = if rtl { Direction::RightToLeft } else { Direction::LeftToRight };
                Rc::new(ShapePlan::new(
                    &self.font,
                    dir,
                    Some(script),
                    None,
                    &self.features[usize::from(!kerning)],
                ))
            })
            .clone()
    }

    /// The advance the system gives `glyph`, points ([`FontSource::advance`]).
    fn advance(&self, glyph: u16) -> f64 {
        *self.advances.borrow_mut().entry(glyph).or_insert_with(|| self.system.advance(glyph))
    }

    /// What `trak` adds to a glyph's advance at this size: a space shaped alone, unkerned, less its own advance.
    fn trak(&self, font: &ShaperFont<'_, '_>) -> i32 {
        *self.trak.get_or_init(|| {
            let mut buf = Buffer::new();
            buf.push(u32::from(' '), 0);
            buf.set_direction(Direction::LeftToRight);
            buf.set_script(Some(Script::COMMON));
            let plan = self.plan(Script::COMMON, false, false);
            let options = ShapeOptions::new()
                .plan(Some(&plan))
                .features(&self.features[1])
                .point_size(Some(self.size));
            if harfrust::shape(font, &mut buf, options).is_err() {
                return 0;
            }
            let (info, pos) = (buf.glyph_infos()[0], buf.glyph_positions()[0]);
            pos.x_advance - font.glyph_h_advance(harfrust::GlyphId::new(info.glyph_id))
        })
    }
}

/// One glyph of a terminal run as HarfRust shaped it: its id, the cell (cluster) it came from, and its advance and
/// offsets, points.
#[derive(Clone, Copy, Debug)]
pub struct RunGlyph {
    pub id: u16,
    pub cell: u32,
    pub adv: f32,
    pub dx: f32,
    pub dy: f32,
}

/// A terminal row run (`cells`, one grapheme each, all set in `system`) shaped by HarfRust with contextual alternates
/// and ligatures on and kerning off, each glyph keeping the index of the cell it came from as its cluster: a ligature
/// over N cells is one glyph (or N glyphs, as JetBrains Mono sets them) at the run's first cell of it.
pub(crate) fn shape_cells(
    fonts: &mut super::Fonts,
    system: &SystemFont,
    cells: &[&str],
) -> Vec<RunGlyph> {
    let Some(inst) = instance(fonts, system) else { return Vec::new() };
    let mut feats = inst.features[1].clone();
    feats.push(Feature::new(Tag::new(b"calt"), 1, ..));
    feats.push(Feature::new(Tag::new(b"liga"), 1, ..));
    let mut buf = Buffer::new();
    buf.set_direction(Direction::LeftToRight);
    buf.set_script(Some(Script::LATIN));
    buf.set_cluster_level(ClusterLevel::MonotoneGraphemes);
    buf.set_flags(
        BufferFlags::BEGINNING_OF_TEXT
            | BufferFlags::END_OF_TEXT
            | BufferFlags::DO_NOT_INSERT_DOTTED_CIRCLE,
    );
    for (i, s) in cells.iter().enumerate() {
        if s.is_empty() {
            buf.push(u32::from(' '), i as u32);
        }
        for c in s.chars() {
            buf.push(u32::from(c), i as u32);
        }
    }
    let plan =
        ShapePlan::new(&inst.font, Direction::LeftToRight, Some(Script::LATIN), None, &feats);
    let font = ShaperFont::new(&inst.font).with_scale(i32::from(inst.font.units_per_em()) * SUB);
    let options =
        ShapeOptions::new().plan(Some(&plan)).features(&feats).point_size(Some(inst.size));
    if harfrust::shape(&font, &mut buf, options).is_err() {
        return Vec::new();
    }
    let s = inst.scale;
    buf.glyph_infos()
        .iter()
        .zip(buf.glyph_positions())
        .map(|(i, p)| RunGlyph {
            id: i.glyph_id as u16,
            cell: i.cluster,
            adv: (f64::from(p.x_advance) * s) as f32,
            dx: (f64::from(p.x_offset) * s) as f32,
            dy: (f64::from(p.y_offset) * s) as f32,
        })
        .collect()
}

/// Whether `system`'s character map has every scalar of `text` that needs a glyph.
pub(crate) fn covers(fonts: &mut super::Fonts, system: &SystemFont, text: &str) -> bool {
    let Some(inst) = instance(fonts, system) else { return false };
    let cmap = inst.font.charmap();
    text.chars().all(|c| invisible(u32::from(c)) || cmap.map_unicode(u32::from(c)).is_some())
}

/// A character that needs no glyph of its own font to be set: controls, joiners, marks of direction, variation
/// selectors and tags, which the characters around them choose the font of.
fn invisible(c: u32) -> bool {
    matches!(c, 0..=0x1F | 0x7F..=0x9F | 0xAD | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0xFE00..=0xFE0F | 0xFEFF | 0xE0000..=0xE0FFF)
}

/// A hard line break (line feed, vertical tab, form feed, carriage return, next line, line and paragraph separators).
const fn breaks_line(c: u32) -> bool {
    matches!(c, 0x0A..=0x0D | 0x85 | 0x2028 | 0x2029)
}

/// The character each shaped glyph stands for. HarfRust's own, but for a glyph a deleted one's cluster was merged into
/// (Lucida Grande sets a qamats under resh as nothing, and the alef after takes the qamats' cluster; its bet-dagesh
/// ligature over a qamats leaves the qamats the dagesh's): a glyph that is just the font's own glyph for a character
/// a little way off that has none of its own is the nearest such character's.
fn owners(
    font: &Font,
    units: &[u16],
    start: usize,
    end: usize,
    infos: &[harfrust::GlyphInfo],
    mut owner: Vec<usize>,
) -> Vec<usize> {
    let cmap = font.charmap();
    let nominal = |u: usize| {
        scalars(units, u, end).next().and_then(|(_, c)| cmap.map_unicode(c)).map(|g| g.to_u32())
    };
    for k in 0..infos.len() {
        let c = owner[k];
        if nominal(c) == Some(infos[k].glyph_id) {
            continue;
        }
        let ahead = scalars(units, (c + 1).min(end), end).take(4).map(|(u, _)| u);
        let behind =
            (c.saturating_sub(8).max(start)..c).filter(|&u| !(0xDC00..0xE000).contains(&units[u]));
        let near = ahead
            .chain(behind)
            .filter(|&u| !owner.contains(&u) && nominal(u) == Some(infos[k].glyph_id))
            .min_by_key(|&u| (u.abs_diff(c), u > c));
        if let Some(u) = near {
            owner[k] = u;
        }
    }
    owner
}

/// Units past a line's edge its edge items are shaped on into: as far as any contextual lookup here looks.
const REACH: usize = 16;

/// How far apart CoreText's default tab stops are, points from the line's start.
const TAB: f64 = 28.0;

/// The text's scalars from unit `a` to `b`, each with the unit it starts at (a lone surrogate is U+FFFD).
fn scalars(units: &[u16], a: usize, b: usize) -> impl Iterator<Item = (usize, u32)> + '_ {
    let mut at = a;
    char::decode_utf16(units[a..b].iter().copied()).map(move |c| {
        let i = at;
        let c = c.map_or(0xFFFD, u32::from);
        at += if c > 0xFFFF { 2 } else { 1 };
        (i, c)
    })
}

/// The script HarfRust shapes `s` as (ISO 15924), Common when it has none.
fn script_of(s: Uscript) -> Script {
    let name = PropertyNamesShort::<Uscript>::new().get(s);
    name.and_then(|n| <[u8; 4]>::try_from(n.as_bytes()).ok())
        .and_then(|t| Script::from_iso15924_tag(Tag::new(&t)))
        .unwrap_or(Script::COMMON)
}

/// A glyph as shaped, before it is placed: its face, id and unit, its advance (kern included), how much of that is
/// space set after the glyph's own advance (tracking, kerning, the kern), and its offset, points.
#[derive(Clone, Copy)]
struct Shaped {
    face: u16,
    id: u16,
    idx: usize,
    adv: f64,
    space: f64,
    dx: f64,
    dy: f64,
}

/// A text in its fonts, every unit's face, script, direction and break opportunity found once.
pub(super) struct Typesetter {
    units: Vec<u16>,
    /// Each unit's face (an index into `faces`), its fallback chosen, and the kern after it.
    face: Vec<u16>,
    faces: Vec<Rc<Instance>>,
    kern: Vec<f32>,
    script: Vec<Script>,
    classes: Vec<BidiClass>,
    levels: Vec<Level>,
    /// Each paragraph's units and level.
    paras: Vec<(usize, usize, Level)>,
    /// Before each unit and after the last: a line may break there, and a grapheme cluster starts there.
    opportunity: Vec<bool>,
    grapheme: Vec<bool>,
    /// Each unit's advance with each paragraph set whole: what a line is measured by before it is set.
    advances: OnceCell<Vec<f64>>,
    /// Each paragraph as a line, set once: it is both measured and set.
    whole: RefCell<HashMap<(usize, usize), Rc<SetLine>>>,
    /// Lines cut from a paragraph, by range, at most [`CUT_LINES`]: a resize asks for the same ones width after width.
    cut: RefCell<HashMap<(usize, usize), Rc<SetLine>>>,
}

/// Cut lines a typesetter keeps; past this it forgets them all and starts again.
const CUT_LINES: usize = 64;

/// Before each unit and after the last, whether a line may break there: UAX #14 as ICU4X has it, with LB20a, which
/// ICU4X does not apply: a word-initial hyphen keeps to its word (`a -b`). Where CoreText breaks against UAX #14 (after
/// a space before `)`, `/`, `.` or a word joiner; after a `”` that follows a space) this does not (docs/text-layout.md,
/// "Where den's typesetter differs from CoreText"). Code breaks only after white space, as an editor wraps it:
/// UAX #14 parts `|>` after its `|`, and CoreText puts a `{` after a space on the next line; a token wider than the
/// line is cut at a grapheme.
fn opportunities(units: &[u16], code: &[bool]) -> Vec<bool> {
    let n = units.len();
    let mut opportunity = vec![false; n + 1];
    for p in LineSegmenter::new_dictionary(LineBreakOptions::default()).segment_utf16(units) {
        opportunity[p] = true;
    }
    opportunity[0] = false;
    type L = LineBreak;
    let classes = CodePointMapData::<LineBreak>::new();
    let at = |i: usize| {
        char::decode_utf16(units[i..].iter().copied())
            .next()
            .and_then(Result::ok)
            .map_or(0xFFFD, u32::from)
    };
    // The scalar that ends before unit `i`.
    let before = |i: usize| match i {
        0 => None,
        _ if i >= 2 && (0xDC00..0xE000).contains(&units[i - 1]) => Some(at(i - 2)),
        _ => Some(at(i - 1)),
    };
    let class = |c: Option<u32>| c.map(|c| classes.get32(c));
    let ends = |c: L| matches!(c, L::SP | L::BK | L::CR | L::LF | L::NL | L::ZW);
    let mut p = 1;
    while p < n {
        let c = at(p);
        let this = classes.get32(c);
        let prev = before(p);
        if code.get(p).copied().unwrap_or(false) && code[p - 1] {
            let white = |c: Option<u32>| matches!(c, Some(0x20 | 0x09));
            opportunity[p] = class(prev)
                .is_some_and(|c| matches!(c, L::BK | L::CR | L::LF | L::NL))
                || white(prev) && !white(Some(c));
            p += if c > 0xFFFF { 2 } else { 1 };
            continue;
        }
        if (class(prev) == Some(L::HY) || prev == Some(0x2010))
            && this == L::AL
            && class(before(p - 1)).is_none_or(|a| ends(a) || matches!(a, L::GL | L::CB))
        {
            opportunity[p] = false;
        }
        p += if c > 0xFFFF { 2 } else { 1 };
    }
    opportunity
}

impl Typesetter {
    pub(super) fn new(fonts: &mut super::Fonts, units: Vec<u16>, attrs: &Attrs) -> Self {
        let n = units.len();
        let opportunity = opportunities(&units, &attrs.code);
        let mut grapheme = vec![false; n + 1];
        for p in GraphemeClusterSegmenter::new().segment_utf16(&units) {
            grapheme[p] = true;
        }
        let bidi = unicode_bidi::utf16::BidiInfo::new(&units, None);
        let paras = bidi.paragraphs.iter().map(|p| (p.range.start, p.range.end, p.level)).collect();
        let (classes, levels) = (bidi.original_classes, bidi.levels);

        let scripts = CodePointMapData::<Uscript>::new();
        let mut script = vec![Script::COMMON; n];
        let mut last: Option<Script> = None;
        let mut pending = 0;
        for (i, c) in scalars(&units, 0, n) {
            let s = scripts.get32(c);
            let w = if c > 0xFFFF { 2 } else { 1 };
            if matches!(s, Uscript::Common | Uscript::Inherited | Uscript::Unknown) {
                if let Some(l) = last {
                    script[i..i + w].fill(l);
                }
                continue;
            }
            let s = script_of(s);
            if last.is_none() {
                script[pending..i].fill(s);
            }
            pending = i + w;
            last = Some(s);
            script[i..i + w].fill(s);
        }

        let mut faces: Vec<Rc<Instance>> = Vec::new();
        let index = |fonts: &mut super::Fonts,
                     faces: &mut Vec<Rc<Instance>>,
                     f: &SystemFont|
         -> Option<u16> {
            let inst = instance(fonts, f)?;
            Some(match faces.iter().position(|f| Rc::ptr_eq(f, &inst)) {
                Some(k) => k as u16,
                None => {
                    faces.push(inst);
                    (faces.len() - 1) as u16
                }
            })
        };
        let spans: Vec<Option<u16>> =
            attrs.fonts.iter().map(|f| index(fonts, &mut faces, f)).collect();
        let presents = CodePointSetData::new::<EmojiPresentation>();
        let mut face = vec![0u16; n];
        let mut g = 0;
        while g < n {
            let mut h = g + 1;
            while h < n && !grapheme[h] {
                h += 1;
            }
            let span = usize::from(attrs.font[g]);
            let base = spans[span];
            let cluster = &units[g..h];
            let (mut covered, mut emoji, mut text_style) = (true, false, false);
            for (_, c) in scalars(&units, g, h) {
                emoji |= c == 0xFE0F || presents.contains32(c);
                text_style |= c == 0xFE0E;
                if !invisible(c) {
                    covered &= base.is_some_and(|b| {
                        faces[usize::from(b)].font.charmap().map_unicode(c).is_some()
                    });
                }
            }
            let chosen = if covered && !(emoji && !text_style) {
                base
            } else {
                let asked = &attrs.fonts[span];
                let id = fonts.register(asked);
                let fb = fonts
                    .shaper
                    .fallback
                    .entry((id, cluster.to_vec()))
                    .or_insert_with(|| {
                        let text = String::from_utf16_lossy(cluster);
                        // Private-use symbols (Nerd Font icons) are the bundled Symbols Nerd Font's, which CoreText's
                        // cascade does not know.
                        let private = text
                            .chars()
                            .all(|c| matches!(u32::from(c), 0xE000..=0xF8FF | 0xF0000..));
                        private
                            .then(|| {
                                super::register_term_fonts();
                                SystemFont::named(super::NERD_FONT, asked.size())
                            })
                            .flatten()
                            .unwrap_or_else(|| asked.fallback(&text))
                    })
                    .clone();
                index(fonts, &mut faces, &fb).or(base)
            };
            face[g..h].fill(chosen.unwrap_or(0));
            g = h;
        }
        if faces.is_empty() {
            // No font could be read: the text sets as nothing, which [`Typesetter::shape`] knows by no faces.
            face.clear();
        }
        Self {
            units,
            face,
            faces,
            kern: attrs.kern.clone(),
            script,
            classes,
            levels,
            paras,
            opportunity,
            grapheme,
            advances: OnceCell::new(),
            whole: RefCell::new(HashMap::new()),
            cut: RefCell::new(HashMap::new()),
        }
    }

    /// The paragraph unit `i` is in: its start, end and level.
    fn para(&self, i: usize) -> (usize, usize, Level) {
        let k = self.paras.partition_point(|p| p.1 <= i).min(self.paras.len().saturating_sub(1));
        self.paras.get(k).copied().unwrap_or((0, self.units.len(), Level::ltr()))
    }

    /// The units `a..b` shaped as one run of one face, script and direction, with the units of `ctx` around them as
    /// context: the glyphs of the units `keep`, appended to `out` in visual order.
    fn shape(
        &self,
        a: usize,
        b: usize,
        keep: std::ops::Range<usize>,
        ctx: (usize, usize),
        rtl: bool,
        out: &mut Vec<Shaped>,
    ) {
        if self.faces.is_empty() {
            return;
        }
        let face = self.face[a];
        let inst = &self.faces[usize::from(face)];
        let kerning = self.kern[a] != 0.0;
        let script = self.script[a];
        let mut buf = Buffer::new();
        buf.set_direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight });
        buf.set_script(Some(script));
        // Each glyph keeps the character it came from, a mark or a reordered vowel too, as CoreText's string indices do.
        buf.set_cluster_level(ClusterLevel::Characters);
        let mut flags = BufferFlags::DO_NOT_INSERT_DOTTED_CIRCLE;
        if a == ctx.0 {
            flags |= BufferFlags::BEGINNING_OF_TEXT;
        }
        if b == ctx.1 {
            flags |= BufferFlags::END_OF_TEXT;
        }
        buf.set_flags(flags);
        let pre: Vec<u32> = scalars(&self.units, ctx.0, a).map(|(_, c)| c).collect();
        buf.set_pre_context_codepoints(&pre[pre.len().saturating_sub(5)..]);
        // Right to left, clusters count down: AAT deletes a glyph with the buffer reversed, and HarfRust merges a deleted
        // glyph's cluster into the glyph before it in the buffer when that one's is larger, so counting up would hand
        // Geeza Pro's lam-alef ligature's spare alef to the letter after it (the khah of للاختبار would stand for the alef).
        let cluster = |i: usize| if rtl { (b - i) as u32 } else { i as u32 };
        for (i, c) in scalars(&self.units, a, b) {
            // CoreText sets a tab and a line end in the font's space (placed in [`SetLine::place`]).
            buf.push(if c == 9 || breaks_line(c) { 0x20 } else { c }, cluster(i));
        }
        let post: Vec<u32> = scalars(&self.units, b, ctx.1).take(5).map(|(_, c)| c).collect();
        buf.set_post_context_codepoints(&post);
        let plan = inst.plan(script, rtl, kerning);
        let font =
            ShaperFont::new(&inst.font).with_scale(i32::from(inst.font.units_per_em()) * SUB);
        let options = ShapeOptions::new()
            .plan(Some(&plan))
            .features(&inst.features[usize::from(!kerning)])
            .point_size(Some(inst.size));
        if harfrust::shape(&font, &mut buf, options).is_err() {
            return;
        }
        let trak = inst.trak(&font);
        let (infos, positions) = (buf.glyph_infos(), buf.glyph_positions());
        let units: Vec<usize> = infos
            .iter()
            .map(|i| if rtl { b - i.cluster as usize } else { i.cluster as usize })
            .collect();
        let clusters = owners(&inst.font, &self.units, a, b, infos, units);
        let grapheme = |k: usize| (0..=clusters[k]).rev().find(|&u| self.grapheme[u]).unwrap_or(0);
        let from = out.len();
        let mut kerned = Vec::new();
        for (k, (info, pos)) in infos.iter().zip(positions).enumerate() {
            let idx = clusters[k];
            // CoreText sets nothing for a character it hides (a joiner, a mark of direction), not even its kern.
            if !keep.contains(&idx) || pos.x_advance == 0 && self.hidden(idx) {
                continue;
            }
            // A grapheme is kerned once, after its last glyph in visual order (right to left its marks come before its
            // base): a mark sits on its base as though the base were not.
            let next = Some(k + 1).filter(|&j| j < infos.len());
            let last = next.is_none_or(|j| grapheme(j) != grapheme(k));
            let kern = if last { f64::from(self.kern[idx]) } else { 0.0 };
            // A glyph's own advance is CoreText's; what shaping added to it (kerning, a mark's attachment) is HarfRust's.
            let (adv, space, added) = if pos.x_advance == 0 {
                (kern, kern, 0.0)
            } else {
                let added = f64::from(
                    pos.x_advance
                        - font.glyph_h_advance(harfrust::GlyphId::new(info.glyph_id))
                        - trak,
                ) * inst.scale;
                (inst.advance(info.glyph_id as u16) + added + kern, added + kern, added)
            };
            out.push(Shaped {
                face,
                id: info.glyph_id as u16,
                idx,
                adv,
                space,
                dx: f64::from(pos.x_offset) * inst.scale,
                dy: f64::from(pos.y_offset) * inst.scale,
            });
            kerned.push(added);
        }
        // Right to left, HarfRust sets a kern on the glyph before the pair's gap in the text as advance and an offset as
        // large, so the glyph moves into the gap on its left: the kern is space after the glyph on that side, which is
        // where a caret halves it (Geeza Pro's ر before ح, 1.4 points): it goes to the next grapheme's glyph on that
        // side, past the glyph's own marks, which keep their places.
        if rtl {
            let start = |i: usize| (0..=i).rev().find(|&u| self.grapheme[u]).unwrap_or(0);
            for k in from + 1..out.len() {
                let d = out[k].dx;
                if d == 0.0 || out[k].adv == 0.0 || (kerned[k - from] - d).abs() >= 1.0e-6 {
                    continue;
                }
                let Some(j) = (from..k).rev().find(|&j| start(out[j].idx) != start(out[k].idx))
                else {
                    continue;
                };
                out[k].adv -= d;
                out[k].space -= d;
                out[k].dx = 0.0;
                out[j].adv += d;
                out[j].space += d;
                for m in &mut out[j + 1..k] {
                    m.dx -= d;
                }
            }
        }
    }

    /// Whether the scalar at unit `i` is set as nothing: invisible, and not a tab or a line end (set as a space).
    fn hidden(&self, i: usize) -> bool {
        scalars(&self.units, i, (i + 2).min(self.units.len()))
            .next()
            .is_some_and(|(_, c)| invisible(c) && c != 9 && !breaks_line(c))
    }

    /// The levels of the units `s..e` set as a line (UAX #9's L1: the white space before a tab or a paragraph's end
    /// and at the line's end takes the paragraph's level).
    fn line_levels(&self, s: usize, e: usize, para: Level) -> Vec<Level> {
        let mut levels = self.levels[s..e].to_vec();
        let mut trailing = true;
        for i in (s..e).rev() {
            match self.classes[i] {
                BidiClass::B | BidiClass::S => {
                    levels[i - s] = para;
                    trailing = true;
                }
                BidiClass::WS
                | BidiClass::FSI
                | BidiClass::LRI
                | BidiClass::RLI
                | BidiClass::PDI
                | BidiClass::BN
                    if trailing =>
                {
                    levels[i - s] = para
                }
                _ => trailing = false,
            }
        }
        levels
    }

    /// The units `s..e` set as a line: shaped on their own, in visual order.
    fn set(&self, s: usize, e: usize) -> SetLine {
        let (_, _, para) = self.para(s);
        let levels = self.line_levels(s, e, para);
        // Runs of one level, in logical order, then reordered (UAX #9's L2).
        let mut runs: Vec<(usize, usize, Level)> = Vec::new();
        for (k, &l) in levels.iter().enumerate() {
            match runs.last_mut() {
                Some(r) if r.2 == l => r.1 = s + k + 1,
                _ => runs.push((s + k, s + k + 1, l)),
            }
        }
        let max = runs.iter().map(|r| r.2.number()).max().unwrap_or(0);
        let min_odd = runs.iter().map(|r| r.2.number()).filter(|l| l % 2 == 1).min();
        if let Some(min_odd) = min_odd {
            for level in (min_odd..=max).rev() {
                let mut i = 0;
                while i < runs.len() {
                    if runs[i].2.number() >= level {
                        let mut j = i;
                        while j < runs.len() && runs[j].2.number() >= level {
                            j += 1;
                        }
                        runs[i..j].reverse();
                        i = j;
                    } else {
                        i += 1;
                    }
                }
            }
        }
        let mut shaped = Vec::new();
        let same = |i: usize, j: usize| {
            self.face[j] == self.face[i]
                && self.script[j] == self.script[i]
                && (self.kern[j] != 0.0) == (self.kern[i] != 0.0)
        };
        let low = |i: usize| self.units.get(i).is_some_and(|u| (0xDC00..0xE000).contains(u));
        for &(a, b, level) in &runs {
            let rtl = level.is_rtl();
            // Items: runs of one paragraph, face, script and kerning, never splitting a scalar.
            let mut items: Vec<(usize, usize)> = Vec::new();
            for (i, c) in scalars(&self.units, a, b) {
                let w = if c > 0xFFFF { 2 } else { 1 };
                match items.last_mut() {
                    Some(it) if same(it.0, i) && self.para(it.0).0 == self.para(i).0 => {
                        it.1 = i + w
                    }
                    _ => items.push((i, i + w)),
                }
            }
            if rtl {
                items.reverse();
            }
            for (x, y) in items {
                // CoreText sets a line from its start on as its paragraph goes on: an item the line's end cuts is shaped
                // on past the end and only its glyphs within the line kept, so a contextual alternate that looks ahead
                // (Berkeley Mono's `|>` broken after the `|`) is the paragraph's; the line's start looks back at nothing.
                let (pa, pb, _) = self.para(x);
                let mut y1 = y;
                while y1 < pb && y1 - y < REACH && same(x, y1) && self.levels[y1] == self.levels[x]
                {
                    y1 += 1;
                }
                if low(y1) {
                    y1 += 1;
                }
                self.shape(x, y1, x..y, (pa, pb), rtl, &mut shaped);
            }
        }
        SetLine::place(self, s, e, &levels, &shaped)
    }

    /// Each unit's advance with each paragraph set whole.
    fn advances(&self) -> &[f64] {
        self.advances.get_or_init(|| {
            let mut adv = vec![0.0; self.units.len()];
            for &(a, b, _) in &self.paras {
                let line = self.whole_line(a, b);
                for (g, w) in line.read.glyphs.iter().zip(&line.read.adv) {
                    adv[g.idx] += w;
                }
            }
            adv
        })
    }

    /// The paragraph `a..b` as one line, set once.
    fn whole_line(&self, a: usize, b: usize) -> Rc<SetLine> {
        if let Some(l) = self.whole.borrow().get(&(a, b)) {
            return l.clone();
        }
        let l = Rc::new(self.set(a, b));
        self.whole.borrow_mut().insert((a, b), l.clone());
        l
    }
}

/// Points either way a line may run past `width` and still fit. A width comes in as an `f32` (a line's measured width,
/// a layout's rectangle less its padding), rounded from the sum of advances it was measured as, so a text given its own
/// width can be a few of an `f32`'s last bits narrower than it is: it fits all the same.
fn slack(width: f64) -> f64 {
    (width.abs() * f64::from(f32::EPSILON) * 4.0).max(1.0e-9)
}

impl Typesetter {
    /// How many units from `start` the line that begins there takes at `width` points (one at least).
    pub(super) fn suggest_line_break(&self, start: usize, width: f64) -> usize {
        let n = self.units.len();
        if start >= n {
            return 1;
        }
        let adv = self.advances();
        let room = width + slack(width);
        let (mut total, mut solid, mut fit) = (0.0, 0.0, None);
        let mut i = start;
        while i < n {
            total += adv[i];
            if !hangs(self.units[i]) {
                solid = total;
            }
            i += 1;
            if solid > room {
                break;
            }
            if self.opportunity[i] {
                fit = Some(i);
                if breaks(self.units[i - 1]) {
                    break;
                }
            }
        }
        if let Some(f) = fit {
            return f - start;
        }
        if i >= n && solid <= room {
            return n - start;
        }
        // No break fits: the line is cut at the last grapheme that fits, and holds one at least.
        let (mut total, mut cut) = (0.0, None);
        for (j, a) in adv.iter().enumerate().take(n).skip(start) {
            total += a;
            if self.grapheme[j + 1] {
                if total > room && cut.is_some() {
                    break;
                }
                cut = Some(j + 1);
            }
        }
        cut.unwrap_or(n) - start
    }

    /// The `len` units from `start` set as a line.
    pub(super) fn line(&self, start: usize, len: usize) -> Rc<SetLine> {
        let (a, b, _) = self.para(start);
        if (start, start + len) == (a, b) {
            return self.whole_line(a, b);
        }
        let key = (start, start + len);
        if let Some(l) = self.cut.borrow().get(&key) {
            return l.clone();
        }
        let l = Rc::new(self.set(key.0, key.1));
        let mut cut = self.cut.borrow_mut();
        if cut.len() >= CUT_LINES {
            cut.clear();
        }
        cut.insert(key, l.clone());
        l
    }
}

/// A line set by the typesetter: its glyphs, and the caret before each of its units.
pub(super) struct SetLine {
    read: Read,
    start: usize,
    /// For each unit from the line's start to its end, both included: where the caret before it sits.
    carets: Vec<f64>,
    /// The units a caret can sit before (grapheme starts, and the line's end).
    stops: Vec<usize>,
    /// Where the direction turns: each unit's second caret, the unit, and the side of it its grapheme lies on; and the
    /// side of each such unit's first caret.
    alts: Vec<(f64, usize, i8)>,
    sides: HashMap<usize, i8>,
}

/// A value for some of a line's units `from..=to`, by unit: what [`SetLine::place`] keeps per grapheme, without hashing.
struct Units<T> {
    from: usize,
    at: Vec<Option<T>>,
}

impl<T> Units<T> {
    fn new(from: usize, to: usize) -> Self {
        Self { from, at: std::iter::repeat_with(|| None).take(to + 1 - from).collect() }
    }

    fn get(&self, u: &usize) -> Option<&T> {
        self.at[*u - self.from].as_ref()
    }

    fn get_mut(&mut self, u: &usize) -> Option<&mut T> {
        self.at[*u - self.from].as_mut()
    }

    fn contains_key(&self, u: &usize) -> bool {
        self.get(u).is_some()
    }

    fn insert(&mut self, u: usize, v: T) {
        self.at[u - self.from] = Some(v);
    }

    /// The value at `u`, `v` put there first if it has none.
    fn or_insert(&mut self, u: usize, v: T) -> &mut T {
        self.at[u - self.from].get_or_insert(v)
    }

    /// The units with a value, in order.
    fn keys(&self) -> impl Iterator<Item = usize> + '_ {
        self.at.iter().enumerate().filter(|(_, v)| v.is_some()).map(|(k, _)| self.from + k)
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.at.iter_mut().flatten()
    }
}

impl<T> std::ops::Index<&usize> for Units<T> {
    type Output = T;

    fn index(&self, u: &usize) -> &T {
        self.get(u).expect("a unit with a value")
    }
}

impl SetLine {
    /// The glyphs `shaped` (visual order) of units `s..e` placed from the line's start.
    fn place(ts: &Typesetter, s: usize, e: usize, levels: &[Level], shaped: &[Shaped]) -> Self {
        let mut read = Read {
            glyphs: Vec::with_capacity(shaped.len()),
            xs: Vec::with_capacity(shaped.len()),
            adv: Vec::with_capacity(shaped.len()),
            ltr: levels.iter().all(|l| l.is_ltr()),
            width: 0.0,
            full: 0.0,
        };
        // Each grapheme's extent (by the unit it starts at), from where its first glyph starts to where its last ends; a
        // ligature's is its first grapheme's. A caret between two glyphs sits halfway into the space set after the first,
        // as CoreText puts it.
        let mut extent: Units<(f64, f64)> = Units::new(s, e);
        let (mut pen, mut half) = (0.0f64, 0.0f64);
        // Where each cluster's glyphs end with no space after them: where the line's end puts the caret.
        let mut bare: Units<(f64, f64)> = Units::new(s, e);
        // Where each grapheme's glyphs start and end, the space after them in.
        let mut raw: Units<(f64, f64)> = Units::new(s, e);
        // Where the pen is after each grapheme's last glyph, and where its first glyph with width is drawn.
        let mut after: Units<f64> = Units::new(s, e);
        let mut drawn: Units<f64> = Units::new(s, e);
        let key = |i: usize| (s..=i).rev().find(|&u| ts.grapheme[u]).unwrap_or(s);
        // Each grapheme's last glyph (visual order), and the space set after its glyphs.
        let mut last: Units<(usize, f64)> = Units::new(s, e);
        // Whether a grapheme's last glyph in the text is its base: right to left, its first glyph in visual order.
        let mut based: Units<bool> = Units::new(s, e);
        for (k, g) in shaped.iter().enumerate() {
            let c = u32::from(ts.units[g.idx]);
            let e = last.or_insert(key(g.idx), (k, 0.0));
            *e = (k, e.1 + if c == 9 || breaks_line(c) { 0.0 } else { g.space });
            let rtl = levels[g.idx - s].is_rtl();
            if !rtl || !based.contains_key(&key(g.idx)) {
                based.insert(key(g.idx), g.idx == key(g.idx));
            }
        }
        for (k, g) in shaped.iter().enumerate() {
            let mut g = *g;
            match u32::from(ts.units[g.idx]) {
                // A tab reaches the next stop; a line end takes no room.
                9 => (g.adv, g.space) = (((pen / TAB).floor() + 1.0) * TAB - pen, 0.0),
                c if breaks_line(c) => (g.adv, g.space) = (0.0, 0.0),
                _ => {}
            }
            let id = ts.faces[usize::from(g.face)].id;
            read.glyphs.push(Glyph {
                font: id,
                id: g.id,
                x: (pen + g.dx) as f32,
                y: g.dy as f32,
                idx: g.idx,
            });
            read.xs.push(pen + g.dx);
            read.adv.push(g.adv);
            let key = key(g.idx);
            // Before a glyph with no width (a line end) the caret is at the glyph. The space after a grapheme is
            // halved as after one glyph if its base ends it (a glyph alone, a conjunct after its vowel); a grapheme a
            // mark or a vowel ends has the caret past its space.
            let space = match last[&key] {
                (j, space) if j == k && based[&key] => space,
                _ => 0.0,
            };
            let from = if g.adv == 0.0 { pen } else { pen - half };
            let to = pen + g.adv - space * 0.5;
            let e = extent.or_insert(key, (from, to));
            e.0 = e.0.min(from);
            e.1 = e.1.max(to);
            let b = bare.or_insert(key, (pen, pen + g.adv - space));
            b.0 = b.0.min(pen);
            b.1 = b.1.max(pen + g.adv - space);
            let r = raw.or_insert(key, (pen, pen + g.adv));
            r.0 = r.0.min(pen);
            r.1 = r.1.max(pen + g.adv);
            if g.adv > 0.0 {
                drawn.or_insert(key, pen + g.dx);
            }
            pen += g.adv;
            after.insert(key, pen);
            half = space * 0.5;
        }
        read.full = pen;
        // A right-to-left grapheme at the line's right end has its leading edge where its glyphs end, as the line's
        // end does: no space set after it.
        if let Some(g) = shaped.last().filter(|g| levels[g.idx - s].is_rtl()) {
            let k = key(g.idx);
            if let (Some(e), Some(b)) = (extent.get_mut(&k), bare.get(&k)) {
                e.1 = b.1;
            }
        }
        let mut trailing = 0.0;
        let mut t = e;
        while t > s && hangs(ts.units[t - 1]) {
            t -= 1;
        }
        for (g, w) in read.glyphs.iter().zip(&read.adv) {
            if g.idx >= t {
                trailing += w;
            }
        }
        read.width = (pen - trailing) as f32;
        // In a right-to-left paragraph the white space a line ends with hangs off its left, before the origin.
        let shift = if ts.para(s).2.is_rtl() { -trailing } else { 0.0 };
        if shift != 0.0 {
            for (g, x) in read.glyphs.iter_mut().zip(&mut read.xs) {
                g.x += shift as f32;
                *x += shift;
            }
            for e in extent.values_mut().chain(bare.values_mut()).chain(raw.values_mut()) {
                e.0 += shift;
                e.1 += shift;
            }
            for a in after.values_mut().chain(drawn.values_mut()) {
                *a += shift;
            }
        }

        // Carets: each cluster's graphemes share its glyphs' advance evenly (a ligature's carets divide the ligature, not
        // the space around it), from its leading edge.
        let starts: Vec<usize> = extent.keys().collect();
        let mut carets = vec![f64::NAN; e - s + 1];
        let mut trail: Units<f64> = Units::new(s, e);
        let mut stops = Vec::new();
        for (k, &c) in starts.iter().enumerate() {
            let end = starts.get(k + 1).copied().unwrap_or(e);
            let (x0, x1) = extent[&c];
            let (b0, b1) = raw[&c];
            let rtl = levels[c - s].is_rtl();
            let bounds: Vec<usize> =
                (c..end).filter(|&u| u == c || (ts.grapheme[u] && !ts.hidden(u))).collect();
            let n = bounds.len();
            let step = (b1 - b0) / n as f64;
            for (j, &u) in bounds.iter().enumerate() {
                let edge = |j: usize| match (j, rtl) {
                    (0, false) => x0,
                    (0, true) => x1,
                    (j, false) if j == n => x1,
                    (j, true) if j == n => x0,
                    (j, false) => b0 + step * j as f64,
                    (j, true) => b1 - step * j as f64,
                };
                carets[u - s] = edge(j);
                trail.insert(u, edge(j + 1));
                stops.push(u);
            }
        }
        // Where the direction turns a unit has two carets at its runs' edges, no space halved: where the grapheme before
        // it ends, and where its own starts. CoreText's primary is the first in a left-to-right paragraph, the second
        // in a right-to-left one; a point may hit either. Each is kept with the side its grapheme lies on (-1 left).
        let para_rtl = ts.para(s).2.is_rtl();
        let mut alts = Vec::new();
        let mut sides = HashMap::new();
        let mut before: Option<usize> = None;
        for u in s..e {
            if !trail.contains_key(&u) {
                continue;
            }
            if let Some(p) = before.filter(|&p| levels[p - s].is_rtl() != levels[u - s].is_rtl()) {
                let (prtl, urtl) = (levels[p - s].is_rtl(), levels[u - s].is_rtl());
                let ends = match raw.get(&p) {
                    Some(&(x0, x1)) => (if prtl { x0 } else { x1 }, if prtl { 1 } else { -1 }),
                    None => (trail[&p], if prtl { 1 } else { -1 }),
                };
                let starts = match raw.get(&u) {
                    Some(&(x0, x1)) => (if urtl { x1 } else { x0 }, if urtl { -1 } else { 1 }),
                    None => (carets[u - s], if urtl { -1 } else { 1 }),
                };
                let (primary, secondary) = if para_rtl { (starts, ends) } else { (ends, starts) };
                carets[u - s] = primary.0;
                sides.insert(u, primary.1);
                alts.push((secondary.0, u, secondary.1));
            }
            before = Some(u);
        }
        // The line's end: after its last unit, at that unit's trailing edge, no space set after it (past it, as between
        // graphemes, when a mark ends the grapheme; right to left, where its base is drawn, not where a mark's offset
        // left the pen).
        carets[e - s] = match starts.last() {
            Some(&c) => {
                let (x0, x1) = if based[&c] {
                    bare[&c]
                } else {
                    (drawn.get(&c).copied().unwrap_or(bare[&c].0), after[&c])
                };
                if levels[c - s].is_rtl() { x0 } else { x1 }
            }
            None => 0.0,
        };
        stops.push(e);
        // A unit within a grapheme, or a hidden character, has the caret after it.
        for u in (s..e).rev() {
            if carets[u - s].is_nan() {
                carets[u - s] = carets[u - s + 1];
            }
        }
        stops.sort_unstable();
        stops.dedup();
        Self { read, start: s, carets, stops, alts, sides }
    }
}

impl SetLine {
    /// What it holds.
    pub(super) fn read(&self) -> Read {
        self.read.clone()
    }

    /// Where the caret before the unit `index` (counted from the typesetter's start) sits, points from the line's origin.
    pub(super) fn offset(&self, index: usize) -> f64 {
        let i = index.saturating_sub(self.start).min(self.carets.len() - 1);
        self.carets[i]
    }

    /// The stop nearest `x`, as CoreText picks it: of stops at one place, the first if `x` is before it and the last
    /// if not (a point past a line's end is after its line break); halfway between two places, the later.
    pub(super) fn index_at(&self, x: f64) -> Option<usize> {
        let mut places: Vec<(f64, usize, Option<i8>)> = self
            .stops
            .iter()
            .map(|&u| (self.offset(u), u, self.sides.get(&u).copied()))
            .chain(self.alts.iter().map(|&(p, u, side)| (p, u, Some(side))))
            .collect();
        places.sort_by_key(|&(_, u, _)| u);
        let near = places.iter().map(|&(p, _, _)| (p - x).abs()).fold(f64::INFINITY, f64::min);
        let tied: Vec<(f64, usize, Option<i8>)> =
            places.into_iter().filter(|&(p, _, _)| (p - x).abs() <= near + 1.0e-9).collect();
        let at = tied.first()?.0;
        let one_place = tied.iter().all(|&(p, _, _)| (p - at).abs() <= 1.0e-9);
        // Where the direction turns, the unit whose grapheme is on the point's side.
        let side = if x < at { -1 } else { 1 };
        if let Some(&(_, u, _)) = tied.iter().find(|&&(_, _, s)| one_place && s == Some(side)) {
            return Some(u);
        }
        if one_place && x < at { tied.first() } else { tied.last() }.map(|&(_, u, _)| u)
    }
}
