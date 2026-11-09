//! Shaping, line breaking, caret and hit geometry, and glyph rasterisation into the GPU atlas.
//!
//! Copied from den-native's text.rs (github.com/andrewgazelka/den): den's typesetter as it is, with the formula fonts, glyph
//! outlines (Slug), selection plates and the bundled Berkeley Mono cut. Code is set in an installed monospace font.
//!
//! den's own typesetter shapes and breaks (text/shaper.rs: HarfRust, ICU4X and unicode-bidi,
//! fed1eb428d:docs/research/text-shaping.md), on macOS and Linux alike. Choosing fonts and the font a character falls back to, font
//! metrics, advances and rasterising are the platform's ([`font::FontSource`]: CoreText on macOS, fontdb, skrifa and
//! tiny-skia on Linux).
//!
//! A text is shaped once ([`Shaped`]: a typesetter for each run of paragraphs and each hard paragraph's glyphs, none of
//! which depends on the width, and only the pieces an edit touched shaped again) and broken at every width it is shown at
//! ([`Layout::wrap`]): the typesetter still picks each break, and a line is cut from its paragraph's glyphs wherever the
//! typesetter has shown it sets the line exactly so (docs/text-layout.md).
use crate::atlas::Packer;
use crate::math::{Katex, PathSeg};
use font::{Font, FontSource};
use shaper::{SetLine, Typesetter};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

pub mod font;
mod font_mac;
mod nerd;
pub mod nerd_rules;
mod shaper;

pub use shaper::RunGlyph;

/// The proportions a scene sets for type (den's `TypeLook` component, copied as plain data): code's scale, the headings,
/// tracking, and a selected blank line's sliver.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TypeLook {
    pub code_scale: f32,
    pub h1: f32,
    pub h2: f32,
    pub h3: f32,
    pub tracking: f32,
    pub sliver: f32,
    pub blank: f32,
    pub item: f32,
    pub indent: f32,
    pub heading_tracking: f32,
    /// Every character its own glyph: no ligatures or contextual alternates (`liga`, `calt` off), so code's `//` and `->`
    /// read as typed. Off in the app (its fonts' own way); on in videos.
    pub plain: bool,
}

/// Identity of a (font, size) pair, stable across layouts: the PostScript name and the size.
pub type FontId = u64;

/// Horizontal subpixel positions a glyph is rasterised at (quarter pixels).
pub const SUBPX: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub size: f32,
    pub bold: bool,
    /// Line box height in points.
    pub line: f32,
}

/// How a run of text is set, beyond the layout's own style (markdown's emphasis, code and headings).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Face {
    /// The text font at weight 500: the sidebar's open row.
    Medium,
    /// The text font with tabular figures (every digit as wide as the others): a column of numbers.
    Tabular,
    Bold,
    Italic,
    BoldItalic,
    /// Code at its regular weight; its colours carry its roles.
    Code,
    /// Code at another weight (CSS numbers: 500 medium, 600 semibold, 700 bold): a highlight role the scene sets heavier
    /// (a `syn_*` style's `CodeWeight`, [`weigh_code`]). Set on the code font's weight axis at that weight, and always as
    /// wide as the regular instance, so the grid holds.
    CodeWeight(u16),
    /// Code's slant: a comment, the prose the program skips.
    CodeItalic,
    /// Code amid prose (markdown's `` `code` ``): the code face on a chip (the one place prose is monospace, as the site
    /// sets it), a step smaller than the words ([`INLINE_CODE_EM`]), untracked, with room for the chip either side of it
    /// ([`crate::markdown::chip_room`]).
    InlineCode,
    /// An inline component's icon hole and label (`<Path loc="...">blaze</Path>`, [`crate::markdown::InlineTag`]): the UI
    /// face at medium weight, with room for its chip either side as inline code has.
    InlineRef,
    /// Level 1 to 6.
    Heading(u8),
    /// A font by its PostScript name (a font setting's preview, plugins/schema.rs `FONTS`); the text font when this
    /// machine has none of that name.
    Named(&'static str),
}

/// Inline code's size against the words around it: the site's `code { font-size: 0.88em }` (site/styles/base.css), on
/// top of the code face's own scale to the UI face.
pub const INLINE_CODE_EM: f32 = 0.92;

impl Face {
    fn is_code(self) -> bool {
        matches!(self, Face::Code | Face::CodeWeight(_) | Face::CodeItalic | Face::InlineCode)
    }
}

/// `runs` with each stretch of plain code (a [`Face::Code`] run) that `weights` covers at a weight other than 400 set in
/// [`Face::CodeWeight`]: the highlight roles' weights laid over the code they colour. `weights` are UTF-16 ranges of the
/// text with a CSS weight each, in order of their starts (of two that cross, the earlier stands); other faces, a slanted
/// run included, are left as they are. The runs that come back are in order and apart if `runs` were.
#[must_use]
pub fn weigh_code(runs: &[Run], weights: &[(usize, usize, u16)]) -> Vec<Run> {
    if !weights.iter().any(|w| w.2 != 400) {
        return runs.to_vec();
    }
    let mut out = Vec::with_capacity(runs.len());
    for r in runs {
        if r.face != Face::Code {
            out.push(*r);
            continue;
        }
        let mut at = r.start;
        // The stretch that starts last before the run may reach into it.
        let first = weights.partition_point(|w| w.0 < r.start).saturating_sub(1);
        for &(lo, hi, weight) in weights[first..].iter().take_while(|w| w.0 < r.end) {
            let (lo, hi) = (lo.max(at), hi.min(r.end));
            if weight == 400 || lo >= hi {
                continue;
            }
            if lo > at {
                out.push(Run { start: at, end: lo, face: Face::Code });
            }
            out.push(Run { start: lo, end: hi, face: Face::CodeWeight(weight) });
            at = hi;
        }
        if at < r.end {
            out.push(Run { start: at, end: r.end, face: Face::Code });
        }
    }
    out
}

/// A face over a UTF-16 range of a layout's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Run {
    pub start: usize,
    pub end: usize,
    pub face: Face,
}

/// Room a text leaves for something drawn over it (a formula, ui/math.rs): the UTF-16 range it stands in (a no-break
/// space, one unit, which the layout makes `width` points wide), how far it reaches above and below the baseline and the
/// clearance a line keeps beyond that. A line that holds one is as tall as it needs to be, never shorter than the style's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hole {
    pub start: usize,
    pub end: usize,
    pub width: f32,
    pub ascent: f32,
    pub descent: f32,
    pub pad: f32,
}

/// What stands in a text where a [`Hole`] is.
pub const HOLE: char = '\u{00A0}';

/// Text's fonts by (bold, italic): Inter, installed or den's own ([`bundled`]). Bold is Inter's semibold (600).
const TEXT_FONTS: [[&str; 2]; 2] =
    [["Inter-Regular", "Inter-Italic"], ["Inter-SemiBold", "Inter-SemiBoldItalic"]];
/// The text font's medium weight (500): Inter's Medium, installed or den's own.
const MEDIUM_FONT: &str = "Inter-Medium";
/// The Inter faces den ships (Inter 4.1, SIL OFL 1.1, fonts/inter), by PostScript name: a machine with no Inter of its
/// own still sets text in the brand's face.
fn bundled(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        "Inter-Regular" => include_bytes!("../fonts/inter/Inter-Regular.ttf"),
        "Inter-Italic" => include_bytes!("../fonts/inter/Inter-Italic.ttf"),
        "Inter-Medium" => include_bytes!("../fonts/inter/Inter-Medium.ttf"),
        "Inter-SemiBold" => include_bytes!("../fonts/inter/Inter-SemiBold.ttf"),
        "Inter-SemiBoldItalic" => include_bytes!("../fonts/inter/Inter-SemiBoldItalic.ttf"),
        _ => return None,
    })
}

/// Code's face, by PostScript name, the first installed: `$HARNESS_MONO` when set, else Berkeley Mono (installed under its
/// owner's licence; never bundled), JetBrains Mono, SF Mono, Menlo; else the system's fixed-pitch font.
const CODE_FONTS: [&str; 4] =
    ["BerkeleyMono-Regular", "JetBrainsMono-Regular", "SFMono-Regular", "Menlo-Regular"];

/// Code's font at `size` (code's scale already applied), its bold or slanted face when it has one.
fn code_font(size: f32, weight: u16, italic: bool) -> Font {
    let env = std::env::var("HARNESS_MONO").ok();
    let regular = env
        .iter()
        .map(String::as_str)
        .chain(CODE_FONTS)
        .find_map(|n| Font::named(n, size))
        .unwrap_or_else(|| Font::fixed(size));
    styled(regular, size, weight, italic)
}

/// The first of `names` installed, by family (Ghostty's font-family) or PostScript name, at `size`.
fn first_family(names: &[String], size: f32) -> Option<Font> {
    names.iter().find_map(|n| Font::family(n, size).or_else(|| Font::named(n, size)))
}

/// The terminal's font families from the user's Ghostty config (font-family and its -bold, -italic and -bold-italic),
/// each in Ghostty's order; empty: harness's own choice ([`code_font`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TermFamilies {
    pub regular: Vec<String>,
    pub bold: Vec<String>,
    pub italic: Vec<String>,
    pub bold_italic: Vec<String>,
}

/// `regular`'s bold (`weight` 600 and up) or slanted face, when it has one.
fn styled(regular: Font, size: f32, weight: u16, italic: bool) -> Font {
    let bold = weight >= 600;
    if !bold && !italic {
        return regular;
    }
    if italic {
        return regular.italic(size, bold).unwrap_or(regular);
    }
    let name = regular.post_script_name();
    let base = name.rsplit_once('-').map_or(name.as_str(), |(b, _)| b).to_owned();
    Font::named(&format!("{base}-Bold"), size)
        .or_else(|| regular.varied(size, &[(u32::from_be_bytes(*b"wght"), 700.0)]))
        .unwrap_or(regular)
}

// ---- the terminal's fonts ------------------------------------------------------------------------------------------

/// The terminal's bundled faces, written out and registered for the process ([`register_term_fonts`]): JetBrains Mono
/// 2.304 (SIL OFL 1.1, fonts/jetbrains-mono) and Symbols Nerd Font Mono 3.5.1 (fonts/nerd-symbols). Berkeley Mono is
/// used when installed and never bundled.
const TERM_BUNDLED: [(&str, &[u8]); 5] = [
    (
        "JetBrainsMono-Regular.ttf",
        include_bytes!("../fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    ),
    ("JetBrainsMono-Bold.ttf", include_bytes!("../fonts/jetbrains-mono/JetBrainsMono-Bold.ttf")),
    (
        "JetBrainsMono-Italic.ttf",
        include_bytes!("../fonts/jetbrains-mono/JetBrainsMono-Italic.ttf"),
    ),
    (
        "JetBrainsMono-BoldItalic.ttf",
        include_bytes!("../fonts/jetbrains-mono/JetBrainsMono-BoldItalic.ttf"),
    ),
    (
        "SymbolsNerdFontMono-Regular.ttf",
        include_bytes!("../fonts/nerd-symbols/SymbolsNerdFontMono-Regular.ttf"),
    ),
];

/// The character of the bundled Symbols Nerd Font glyph named `name` (its PostScript glyph name, which is the Nerd Fonts
/// cheat sheet's: `cod-check`, `dev-rust`, `seti-typescript`, `fa-gear`), None when the face has no glyph of that name.
/// The names are read from the face's `post` and `cmap` tables once.
#[must_use]
pub fn nerd_symbol(name: &str) -> Option<char> {
    use read_fonts::TableProvider;
    static NAMES: std::sync::OnceLock<HashMap<String, char>> = std::sync::OnceLock::new();
    let names = NAMES.get_or_init(|| {
        let mut by_name = HashMap::new();
        let Some((_, bytes)) = TERM_BUNDLED.iter().find(|(n, _)| n.starts_with("SymbolsNerdFont")) else { return by_name };
        let Ok(font) = read_fonts::FontRef::new(bytes) else { return by_name };
        let (Ok(post), Ok(cmap)) = (font.post(), font.cmap()) else { return by_name };
        let Some((_, _, table)) = cmap.best_subtable() else { return by_name };
        for (cp, gid) in table.iter() {
            let (Some(c), Ok(g)) = (char::from_u32(cp), u16::try_from(gid.to_u32())) else { continue };
            if let Some(n) = post.glyph_name(read_fonts::types::GlyphId16::new(g)) {
                by_name.entry(n.to_owned()).or_insert(c);
            }
        }
        by_name
    });
    names.get(name).copied()
}

/// The Nerd Font symbols face and the colour emoji face, by PostScript name.
const NERD_FONT: &str = "SymbolsNFM";
const EMOJI_FONT: &str = "AppleColorEmoji";

/// Register the bundled terminal faces for this process, once: each written to the user's cache directory (when it is
/// not there already, byte for byte) and registered with `CTFontManagerRegisterFontURLs` at process scope, so CoreText
/// finds JetBrains Mono's bold and italic by family and trait like an installed face's.
pub fn register_term_fonts() {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        let dir = std::env::var_os("HOME")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
            .join("Library/Caches/harness-native/fonts");
        let mut paths = Vec::new();
        for (name, bytes) in TERM_BUNDLED {
            let p = dir.join(name);
            let same = std::fs::metadata(&p).is_ok_and(|m| m.len() == bytes.len() as u64)
                && std::fs::read(&p).is_ok_and(|b| b == bytes);
            if !same {
                if let Err(e) =
                    std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&p, bytes))
                {
                    eprintln!("harness-native: cannot write bundled font {}: {e}", p.display());
                    continue;
                }
            }
            paths.push(p);
        }
        if let Err(e) = font_mac::register_fonts(&paths) {
            eprintln!("harness-native: registering bundled fonts: {e}");
        }
    });
}

/// The primary face's metrics, points, read as Ghostty's CoreText face reads them (src/font/face/coretext.zig
/// `getMetrics`): the widest advance of printable ASCII; ascent, descent (positive, down) and line gap from `hhea`, or
/// OS/2's typo metrics when the font sets USE_TYPO_METRICS or `hhea` has none, or its win metrics; cap height from OS/2;
/// underline position (above the baseline, negative below) and thickness from `post`.
#[derive(Clone, Copy, Debug)]
pub struct FaceMetrics {
    pub advance: f32,
    pub ascent: f32,
    pub descent: f32,
    pub leading: f32,
    pub cap: f32,
    pub underline_pos: Option<f32>,
    pub underline_thick: Option<f32>,
}

/// Big-endian reads from a font table.
fn be_i16(t: &[u8], at: usize) -> Option<f32> {
    t.get(at..at + 2).map(|b| f32::from(i16::from_be_bytes([b[0], b[1]])))
}

fn be_u16(t: &[u8], at: usize) -> Option<u16> {
    t.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn face_metrics(f: &Font) -> FaceMetrics {
    let size = f.size();
    let upem = f
        .table_bytes(b"head")
        .or_else(|| f.table_bytes(b"bhed"))
        .and_then(|h| be_u16(&h, 18))
        .filter(|&u| u > 0)
        .map_or(1000.0, f32::from);
    let k = size / upem;
    let hhea = f.table_bytes(b"hhea");
    let os2 = f.table_bytes(b"OS/2");
    let post = f.table_bytes(b"post");
    let (ascent, descent, leading) = (|| {
        let ct = (f.ascent(), f.descent().abs(), f.leading().max(0.0));
        let Some(h) = hhea.as_deref() else { return ct };
        let (ha, hd, hg) =
            (be_i16(h, 4).unwrap_or(0.0), be_i16(h, 6).unwrap_or(0.0), be_i16(h, 8).unwrap_or(0.0));
        let Some(o) = os2.as_deref() else { return (ha * k, -hd * k, hg * k) };
        let (oa, od, og) = (
            be_i16(o, 68).unwrap_or(0.0),
            be_i16(o, 70).unwrap_or(0.0),
            be_i16(o, 72).unwrap_or(0.0),
        );
        let typo = be_u16(o, 62).is_some_and(|s| s & (1 << 7) != 0);
        if typo {
            return (oa * k, -od * k, og * k);
        }
        if ha != 0.0 || hd != 0.0 {
            return (ha * k, -hd * k, hg * k);
        }
        if oa != 0.0 || od != 0.0 {
            return (oa * k, -od * k, og * k);
        }
        let (wa, wd) = (be_u16(o, 74).map_or(0.0, f32::from), be_u16(o, 76).map_or(0.0, f32::from));
        (wa * k, wd * k, 0.0)
    })();
    let cap = os2
        .as_deref()
        .filter(|o| be_u16(o, 0).is_some_and(|v| v >= 2))
        .and_then(|o| be_i16(o, 88))
        .map_or_else(|| f.cap_height(), |c| c * k);
    let (underline_pos, underline_thick) = post.as_deref().map_or((None, None), |p| {
        let (pos, thick) = (be_i16(p, 8).unwrap_or(0.0), be_i16(p, 10).unwrap_or(0.0));
        let broken = thick == 0.0;
        ((!(broken && pos == 0.0)).then_some(pos * k), (!broken).then_some(thick * k))
    });
    let advance = (32u8..127).map(|c| f.advance(f.glyph(char::from(c))) as f32).fold(0.0, f32::max);
    FaceMetrics {
        advance: if advance > 0.0 { advance } else { size * 0.6 },
        ascent,
        descent,
        leading,
        cap,
        underline_pos,
        underline_thick,
    }
}

impl Fonts {
    /// The terminal's primary face at `size` in its bold and slanted cuts: `$HARNESS_MONO`, else Berkeley Mono when
    /// installed, else the bundled JetBrains Mono.
    pub fn term_font(&mut self, size: f32, bold: bool, italic: bool) -> FontId {
        register_term_fonts();
        let weight = if bold { 700 } else { 400 };
        let fam = &self.term_families;
        // A family the config names for this style, in that style; else the primary family's; else harness's own.
        let own = match (bold, italic) {
            (false, false) => &[][..],
            (true, false) => &fam.bold[..],
            (false, true) => &fam.italic[..],
            (true, true) => &fam.bold_italic[..],
        };
        let f = first_family(own, size)
            .or_else(|| first_family(&fam.regular, size))
            .map_or_else(|| code_font(size, weight, italic), |r| styled(r, size, weight, italic));
        self.register(&f)
    }

    /// Set the terminal's families (the user's Ghostty font-family keys); true when they changed.
    pub fn set_term_families(&mut self, f: TermFamilies) -> bool {
        let changed = self.term_families != f;
        self.term_families = f;
        changed
    }

    /// `id`'s metrics (an unknown id reads as zeros).
    #[must_use]
    pub fn term_metrics(&self, id: FontId) -> FaceMetrics {
        let Some(f) = self.font(id) else {
            return FaceMetrics {
                advance: 0.0,
                ascent: 0.0,
                descent: 0.0,
                leading: 0.0,
                cap: 0.0,
                underline_pos: None,
                underline_thick: None,
            };
        };
        face_metrics(f)
    }

    /// Symbols Nerd Font Mono at `size`, when registered.
    pub fn nerd_font(&mut self, size: f32) -> Option<FontId> {
        register_term_fonts();
        let f = Font::named(NERD_FONT, size)?;
        Some(self.register(&f))
    }

    /// Apple Color Emoji at `size`.
    pub fn emoji_font(&mut self, size: f32) -> Option<FontId> {
        let f = Font::named(EMOJI_FONT, size)?;
        Some(self.register(&f))
    }

    /// The font CoreText's cascade sets `text` in for `primary`.
    pub fn fallback_font(&mut self, primary: FontId, text: &str) -> Option<FontId> {
        let f = self.font(primary)?.fallback(text);
        Some(self.register(&f))
    }

    /// Whether `id` has a glyph for every scalar of `text` that needs one.
    pub fn covers(&mut self, id: FontId, text: &str) -> bool {
        let Some(f) = self.font(id).cloned() else { return false };
        shaper::covers(self, &f, text)
    }

    /// A terminal row run, one grapheme a cell, shaped in `id` ([`shaper::shape_cells`]).
    pub fn shape_cells(&mut self, id: FontId, cells: &[&str]) -> Vec<RunGlyph> {
        let Some(f) = self.font(id).cloned() else { return Vec::new() };
        shaper::shape_cells(self, &f, cells)
    }

    /// `text`'s glyph in `id` as CoreText sets it (an emoji sequence joined into one glyph), when it sets one.
    #[must_use]
    pub fn ct_glyph(&self, id: FontId, text: &str) -> Option<u16> {
        self.font(id)?.ct_glyphs(text).into_iter().find(|&g| g != 0)
    }

    /// `glyph`'s ink box in `id`, points from its origin (`[x, y, w, h]`, y up).
    #[must_use]
    pub fn glyph_bounds(&self, id: FontId, glyph: u16) -> [f64; 4] {
        self.font(id).map_or([0.0; 4], |f| f.bounds(glyph))
    }

    /// Whether `id`'s glyphs are colour bitmaps (an emoji font).
    #[must_use]
    pub fn is_colour(&self, id: FontId) -> bool {
        self.font(id).is_some_and(FontSource::colour)
    }
}

/// Whether `text` (a grapheme cluster) asks to be drawn as an emoji: an emoji-presentation character or U+FE0F, and no
/// text-presentation selector.
#[must_use]
pub fn emoji_presentation(text: &str) -> bool {
    use icu_properties::CodePointSetData;
    use icu_properties::props::EmojiPresentation;
    let set = CodePointSetData::new::<EmojiPresentation>();
    !text.contains('\u{FE0E}') && text.chars().any(|c| c == '\u{FE0F}' || set.contains(c))
}

/// The code face's underline thickness at `size`, points: box-drawing lines are drawn this thick (boxdraw.rs).
#[must_use]
pub fn mono_underline(fonts: &Fonts, size: f32) -> f32 {
    let t = code_font(size * fonts.look.code_scale, 400, false).underline_thickness();
    if t > 0.0 { t } else { size / 14.0 }
}

/// The font `name` at `size`: the installed one, else den's bundled copy.
fn text_font(name: &str, size: f32) -> Option<Font> {
    Font::named(name, size).or_else(|| bundled(name).and_then(|bytes| Font::from_data(bytes, size)))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    pub font: FontId,
    pub id: u16,
    /// Pen x from the line origin, points.
    pub x: f32,
    /// How far the font lifts the glyph off the baseline, points: a mark stacked over another mark (Arabic harakat, a
    /// pile of combining accents) sits higher than one alone.
    pub y: f32,
    /// UTF-16 index of the glyph's first character.
    pub idx: usize,
}

pub struct Line {
    /// UTF-16 range of the source, newline included.
    pub start: usize,
    pub end: usize,
    pub top: f32,
    /// Line box height in points: the style's, or the code line's inside code.
    pub height: f32,
    pub baseline: f32,
    /// Where the eye puts the middle of the line's letters: halfway between the centres of its capitals and of its
    /// lowercase (its font's cap height and x-height over the baseline), which is what an icon beside it centres on.
    pub mid: f32,
    pub width: f32,
    pub glyphs: Vec<Glyph>,
    /// The line ends in a hard newline (a caret cannot sit after it on this line).
    pub hard_break: bool,
    /// The line as its typesetter set it, for caret and hit geometry: made when the line was, or on first use for a line
    /// cut from its paragraph's glyphs ([`Layout::set_line`]).
    set: OnceCell<Rc<SetLine>>,
    /// The unit of the text `set`'s string index 0 stands for: its typesetter's string is part of the text ([`Piece`]).
    base: usize,
}

impl Line {
    /// Make the line as tall as the holes it holds need (never shorter than it is), keeping its text where it sits
    /// within the room: the baseline drops by what the tallest hole needs above it and the line grows by what it needs below.
    /// `holes` are in order.
    fn fit_holes(&mut self, holes: &[Hole]) {
        let mine = self.holes_in(holes);
        for h in mine {
            // A line cut from its paragraph is left to right: a hole's space starts where its glyph is.
            let at = match self.set.get() {
                Some(set) => Some(set.offset(h.start - self.base) as f32),
                None => self.glyphs.iter().find(|g| g.idx == h.start).map(|g| g.x),
            };
            if let Some(at) = at {
                self.width = self.width.max(at + h.width);
            }
        }
        self.lift(mine);
    }

    /// The holes of `holes` (in order) the line holds.
    fn holes_in<'h>(&self, holes: &'h [Hole]) -> &'h [Hole] {
        let first = holes.partition_point(|h| h.start < self.start);
        &holes[first..first + holes[first..].partition_point(|h| h.start < self.end)]
    }

    /// The height and baseline [`Line::fit_holes`] gives the line for the holes it holds, `mine`.
    fn lift(&mut self, mine: &[Hole]) {
        if mine.is_empty() {
            return;
        }
        let (mut above, mut below) =
            (self.baseline - self.top, self.top + self.height - self.baseline);
        for h in mine {
            above = above.max(h.ascent + h.pad);
            below = below.max(h.descent + h.pad);
        }
        let moved = self.top + above - self.baseline;
        self.baseline += moved;
        self.mid += moved;
        self.height = above + below;
    }

    /// The line `units` further along the text, its box still to be placed.
    fn moved(&self, units: isize) -> Self {
        let at = |i: usize| i.wrapping_add_signed(units);
        Self {
            start: at(self.start),
            end: at(self.end),
            glyphs: self.glyphs.iter().map(|g| Glyph { idx: at(g.idx), ..*g }).collect(),
            set: self.set.clone(),
            base: at(self.base),
            ..*self
        }
    }
}

/// A run of a text broken to a width of its own ([`Layout::wrap_with`]): a line that starts in the UTF-16 range `lo..hi`
/// is broken `width` wide (0 or less: only at hard breaks), whatever the rest of the text is broken to. A code block that
/// keeps its lines whole, or wraps them short of a gutter of numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub lo: usize,
    pub hi: usize,
    pub width: f32,
}

/// The lines starting in the UTF-16 range `lo..hi` of a layout, moved `dx` along their own axis and shown only between
/// the two x of `clip` (layout points, after the move): a code block's lines past its gutter of numbers, scrolled
/// sideways. Within `fade` of an edge the glyphs thin out; an edge with no fade cuts at the glyph's middle. Caret, hit,
/// plates and drawing all take it ([`Layout::set_slides`]), so selection, find and copy are the same wherever it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slide {
    pub lo: usize,
    pub hi: usize,
    pub dx: f32,
    pub clip: [f32; 2],
    pub fade: [f32; 2],
}

impl Slide {
    /// How much of a glyph from `x0` to `x1` (layout points, moved) shows: 0 past an edge, thinning within its fade.
    #[must_use]
    pub fn shows(&self, x0: f32, x1: f32) -> f32 {
        let c = (x0 + x1) * 0.5;
        let edge = |d: f32, fade: f32| {
            if fade > 0.0 { (d / fade).clamp(0.0, 1.0) } else { f32::from(u8::from(d >= 0.0)) }
        };
        edge(c - self.clip[0], self.fade[0]) * edge(self.clip[1] - c, self.fade[1])
    }
}

/// The width a line starting at `s` is broken to: its band's, else `width`.
/// How tall the box is of a line that starts at unit `s` of `units`: a code line's in code (`code_line`, when positive);
/// in markdown text (text with a code line height) a blank line between paragraphs `blank` of the text's line (when
/// positive) to whole points, unless a card is beside it (its padding reaches into it): code, or a card's end marked by
/// an empty range in `code` (a quote's, `Rich::code_ranges`); else the text's line.
fn box_height(
    units: &[u16],
    code: &[(usize, usize)],
    style: Style,
    code_line: f32,
    blank: f32,
    s: usize,
) -> f32 {
    let in_code = |i: usize| code.iter().any(|&(a, b)| (a..b).contains(&i));
    if code_line > 0.0 && in_code(s) {
        return code_line;
    }
    let carded = |i: usize| code.iter().any(|&(a, b)| (a..b).contains(&i) || (a == b && a == i));
    // The line before ends on its break at `s - 1`, its last character (a card's, when one ends there) at `s - 2`.
    let lone_break =
        units.get(s) == Some(&10) && !(1..=2).any(|k| s >= k && carded(s - k)) && !carded(s + 1);
    if code_line > 0.0 && blank > 0.0 && lone_break {
        return (style.line * blank).round().max(1.0);
    }
    style.line
}

fn band_width(bands: &[Band], s: usize, width: f64) -> f64 {
    match bands.iter().find(|b| (b.lo..b.hi).contains(&s)) {
        Some(b) if b.width > 0.0 => f64::from(b.width),
        Some(_) => 1.0e7,
        None => width,
    }
}

pub struct Layout {
    pub lines: Vec<Line>,
    /// The style's line height (code lines may be shorter: see [`Line::height`]).
    pub line_h: f32,
    /// The line height asked for code lines (0: none).
    pub code_line: f32,
    pub len: usize,
    pub size: f32,
    /// Room under the last line that [`Layout::open_gap`] made (a note hung under the last line of a code view).
    pub tail: f32,
    /// A selected blank line's sliver, in ems (`TypeLook::sliver`).
    pub sliver: f32,
    /// The holes the text was laid out with: the caret after a kerned space sits part-way into its kern, so the end of a
    /// hole is measured from its start and its width instead.
    pub holes: Vec<Hole>,
    /// The text shaped, which the layout was broken from: the typesetter's lines for caret and hit geometry are made
    /// from it on first use.
    shaped: Rc<Shaped>,
    /// The width the text was broken to (0 or less: not wrapped), which with `shaped` and `bands` is all it takes to break
    /// it the same way again.
    wrap_w: f32,
    bands: Vec<Band>,
    /// Where runs of its lines are moved and cut ([`Slide`]): set after the layout is made, and again as they scroll.
    slides: RefCell<Vec<Slide>>,
}

/// Fonts by id, so a glyph can be rasterised from the exact font (and fallback) the typesetter chose. The proportions the
/// scene sets for type (`TypeLook`: code's scale, the headings, tracking) are `look`; a default `Fonts` has none (zero
/// sizes) and is for tests that do not set code or headings.
pub struct Fonts {
    by_id: HashMap<FontId, Font>,
    ui: HashMap<(u32, bool), Font>,
    faces: HashMap<(Face, u32), Font>,
    look: TypeLook,
    /// What the typesetter has read of the fonts.
    shaper: shaper::Fonts,
    /// The terminal's families from the user's Ghostty config.
    term_families: TermFamilies,
    /// The bundled KaTeX fonts a formula has asked for, each loaded once at one point to the em ([`Fonts::math_font`]).
    math: HashMap<Katex, Option<FontId>>,
}

impl Default for Fonts {
    /// No proportions set ([`Fonts::new`]).
    fn default() -> Self {
        Self::new(TypeLook::default())
    }
}

fn fnv(s: &str, size: f32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes().chain(size.to_bits().to_le_bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl Fonts {
    /// Fonts that set type in the proportions of `look`.
    #[must_use]
    pub fn new(look: TypeLook) -> Self {
        Self {
            by_id: HashMap::new(),
            ui: HashMap::new(),
            faces: HashMap::new(),
            look,
            shaper: shaper::Fonts::default(),
            term_families: TermFamilies::default(),
            math: HashMap::new(),
        }
    }

    /// Set type in the proportions of `look` from now on (a scene reload): faces made for the old ones are made again.
    pub fn set_look(&mut self, look: TypeLook) {
        if self.look != look {
            self.look = look;
            self.faces.clear();
        }
    }

    fn register(&mut self, f: &Font) -> FontId {
        // A variable font's instances share one PostScript name: where each is set on its axes tells them apart.
        let mut axes = f.variations();
        axes.sort_by_key(|a| a.0);
        let id = axes.into_iter().fold(fnv(&f.post_script_name(), f.size()), |h, (tag, v)| {
            (h ^ (u64::from(tag) << 32 | u64::from(v.to_bits()))).wrapping_mul(0x0100_0000_01b3)
        });
        self.by_id.entry(id).or_insert_with(|| f.clone());
        id
    }

    fn font(&self, id: FontId) -> Option<&Font> {
        self.by_id.get(&id)
    }

    /// The id of the bundled KaTeX font `font`, loaded on first use at one point to the em (so a glyph of it at any size
    /// is its atlas scale); none if it has no file (the fallbacks RaTeX names) or the file does not load.
    pub fn math_font(&mut self, font: Katex) -> Option<FontId> {
        if let Some(id) = self.math.get(&font) {
            return *id;
        }
        let id = crate::math::font_file(font).and_then(|bytes| Font::from_data(bytes, 1.0)).map(|f| self.register(&f));
        self.math.insert(font, id);
        id
    }

    /// The glyph a character is in a font (as [`Fonts::math_font`] gave it), if the font has one.
    #[must_use]
    pub fn math_glyph(&self, font: FontId, ch: char) -> Option<u16> {
        let g = self.by_id.get(&font)?.glyph(ch);
        (g != 0).then_some(g)
    }

    /// How wide `ch` is at `size` points in `face` (the text font, bold or not, when there is none), before any tracking
    /// or kerning.
    fn advance_of(&mut self, size: f32, bold: bool, face: Option<Face>, ch: char) -> f32 {
        let font = match face {
            Some(f) => self.face_font(size, f),
            None => self.ui_font(size, bold),
        };
        font.advance(font.glyph(ch)) as f32
    }

    /// The PostScript name of the font `id` stands for.
    #[cfg(test)]
    pub fn name_of(&self, id: FontId) -> Option<String> {
        self.by_id.get(&id).map(FontSource::post_script_name)
    }

    /// The PostScript name and the size in points of the font `id` stands for: what the system needs to set a word in the
    /// face it is drawn in (the dictionary's card is set from it).
    #[must_use]
    pub fn face_of(&self, id: FontId) -> Option<(String, f32)> {
        self.by_id.get(&id).map(|f| (f.post_script_name(), f.size()))
    }

    fn ui_font(&mut self, size: f32, bold: bool) -> Font {
        self.ui
            .entry((size.to_bits(), bold))
            .or_insert_with(|| {
                text_font(TEXT_FONTS[usize::from(bold)][0], size)
                    .unwrap_or_else(|| Font::system(size, bold))
            })
            .clone()
    }

    /// The size a markdown heading of `level` is set at (4 to 6 as 3).
    fn heading_size(&self, level: u8) -> f32 {
        [self.look.h1, self.look.h2, self.look.h3][usize::from(level.clamp(1, 3) - 1)]
    }

    /// The font a face sets text of `size` in.
    fn face_font(&mut self, size: f32, face: Face) -> Font {
        if let Some(f) = self.faces.get(&(face, size.to_bits())) {
            return f.clone();
        }
        let f = match face {
            Face::Medium => {
                let regular = self.ui_font(size, false);
                text_font(MEDIUM_FONT, size).or_else(|| regular.medium(size)).unwrap_or(regular)
            }
            Face::Tabular => self.ui_font(size, false).tabular(size),
            Face::Bold => self.ui_font(size, true),
            Face::Italic | Face::BoldItalic => {
                let bold = face == Face::BoldItalic;
                text_font(TEXT_FONTS[usize::from(bold)][1], size).unwrap_or_else(|| {
                    let base = self.ui_font(size, bold);
                    base.italic(size, bold).unwrap_or(base)
                })
            }
            Face::Code => code_font(size * self.look.code_scale, 400, false),
            Face::CodeWeight(weight) => code_font(size * self.look.code_scale, weight, false),
            Face::CodeItalic => code_font(size * self.look.code_scale, 400, true),
            Face::InlineCode => code_font(size * self.look.code_scale * INLINE_CODE_EM, 400, false),
            Face::InlineRef => self.face_font(size, Face::Medium),
            Face::Heading(level) => self.ui_font(self.heading_size(level), true),
            Face::Named(name) => text_font(name, size).unwrap_or_else(|| self.ui_font(size, false)),
        };
        self.faces.insert((face, size.to_bits()), f.clone());
        f
    }
}

/// What a text is set in, unit by unit: each unit's font (an index into `fonts`), the kern after it (den's tracking; 0.0
/// also turns the font's own kerning off), and whether it is code (which breaks only after white space).
struct Attrs {
    fonts: Vec<Font>,
    font: Vec<u16>,
    kern: Vec<f32>,
    code: Vec<bool>,
}

/// The typesetter over `text` (`len` UTF-16 units) in `font`, tracked, with `runs` in their faces (code untracked) and
/// each of `holes` a no-break space as wide as the hole.
fn typesetter(
    fonts: &mut Fonts,
    text: &str,
    len: usize,
    runs: &[Run],
    holes: &[Hole],
    style: Style,
    font: &Font,
) -> Typesetter {
    let mut attrs = Attrs {
        fonts: vec![font.clone()],
        font: vec![0; len],
        kern: vec![fonts.look.tracking * style.size; len],
        code: vec![false; len],
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    for r in runs.iter().filter(|r| r.start < r.end.min(len)) {
        let f = fonts.face_font(style.size, r.face);
        let i = attrs.fonts.len() as u16;
        attrs.fonts.push(f);
        attrs.font[r.start..r.end.min(len)].fill(i);
        if r.face.is_code() {
            attrs.kern[r.start..r.end.min(len)].fill(0.0);
            attrs.code[r.start..r.end.min(len)].fill(true);
        }
        if matches!(r.face, Face::InlineCode | Face::InlineRef) {
            // The chip's room: after the unit before the span and after its last unit (runs come in order, so the
            // unit before is already set).
            let (before, after) = crate::markdown::chip_room(&units, r.start, r.end.min(len), style.size);
            if r.start > 0 {
                attrs.kern[r.start - 1] += before;
            }
            attrs.kern[r.end.min(len) - 1] += after;
        } else if let Face::Heading(level) = r.face {
            attrs.kern[r.start..r.end.min(len)]
                .fill(fonts.look.heading_tracking * fonts.heading_size(level));
        }
    }
    for h in holes.iter().filter(|h| h.start < h.end.min(len)) {
        // The space is as wide as the hole: its own advance in the face it stands in, plus a kern for the rest.
        let face =
            runs.iter().rev().find(|r| r.start <= h.start && h.start < r.end).map(|r| r.face);
        let kern = h.width - fonts.advance_of(style.size, style.bold, face, HOLE);
        attrs.kern[h.start..h.end.min(len)].fill(kern);
    }
    Typesetter::new(fonts, text.encode_utf16().collect(), &attrs)
}

/// How much narrower than the width a whole paragraph (trailing space and all) must be to be taken as one line without
/// asking the typesetter.
const FITS: f64 = 1.0;

/// What a line's baseline and optical middle are measured from: the metrics of the font the line asked for, not the
/// line's measured bounds. A fallback font (Burmese, Tibetan, cuneiform) is far taller than the UI font, and measuring it
/// would drift each line's baseline off the grid its neighbours keep; its tall glyphs overhang the line, as in any text view.
#[derive(Clone, Copy, Debug)]
struct Metrics {
    ascent: f32,
    descent: f32,
    rise: f32,
}

impl Metrics {
    fn of(font: &Font) -> Self {
        Self { ascent: font.ascent(), descent: font.descent(), rise: optical_rise(font) }
    }

    /// The baseline below the top of a line box `line` points tall, the text centred in it.
    fn baseline(self, line: f32) -> f32 {
        (line - (self.ascent + self.descent)) * 0.5 + self.ascent
    }
}

/// A text shaped once, for any width, in pieces ([`Piece`]): each typeset on its own, with each hard paragraph in it as
/// the typesetter sets it on one line. [`Layout::wrap`] breaks it to a width; none of it changes with the width, and the text
/// shaped again after an edit ([`Shaped::after`]) keeps every piece it still has.
pub struct Shaped {
    units: Vec<u16>,
    /// Each piece and the unit of the text it starts at, in order, covering the text.
    pieces: Vec<(usize, Rc<Piece>)>,
    runs: Vec<Run>,
    /// The metrics of each run's face, in `runs`' order, and of the text font.
    faces: Vec<Metrics>,
    base: Metrics,
    code: Vec<(usize, usize)>,
    /// The units list items start at that have room above them ([`Shaped::spaced`]), in order.
    items: Vec<usize>,
    holes: Vec<Hole>,
    style: Style,
    code_line: f32,
    look: TypeLook,
}

/// A run of a text through the paragraph separators it ends on, typeset on its own with [`CONTEXT`] units of the text
/// either side of it: the typesetter breaks no line across a paragraph separator and shapes nothing across one, so a
/// piece sets as it does within the text. Its indices are its typesetter's, counting from the start of the context
/// before it.
struct Piece {
    /// Its units and the context's.
    units: Vec<u16>,
    /// How many units of context come before its own, and how many it has.
    lead: usize,
    len: usize,
    runs: Vec<Run>,
    /// Its holes, which shape it by where they are and how wide.
    holes: Vec<Hole>,
    /// A hash of `units`, to find the piece again in the text shaped after an edit.
    key: u64,
    ts: Typesetter,
    /// Its hard paragraphs, its context's not.
    paras: Vec<Para>,
    /// What is known of a cut before each UTF-16 unit (and after the last), as [`Seam`] bits: the typesetter shapes each
    /// line on its own, so a contextual alternate or a kerning pair across a cut is set differently on either side of it.
    seams: Vec<Cell<u8>>,
    /// Its lines as last broken from its start ([`Broken`]).
    broken: RefCell<Option<Broken>>,
}

/// A piece's lines at one width, counted from its own units' start: a text broken again after an edit takes them for
/// every piece whose lines nothing it changed reaches, and places each line's box again ([`Shaped::placed`]).
struct Broken {
    /// What the lines were broken from beyond the piece ([`Shaped::breaks_key`]).
    key: u64,
    lines: Vec<Line>,
}

/// How many UTF-16 units of the text either side of a piece its typesetter sees: more than any contextual lookup or
/// kerning pair reaches across a paragraph separator, and enough that the last unit of a piece is not the last of its
/// string.
const CONTEXT: usize = 32;

/// A paragraph separator that is also a hard line break (line feed, carriage return, next line, paragraph separator): a
/// piece ends after a run of them.
fn separates(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{85}' | '\u{2029}')
}

/// A hard line break: what a line past the end of an overflowing one ends on.
fn breaks(u: u16) -> bool {
    matches!(u, 10..=13 | 0x85 | 0x2028 | 0x2029)
}

/// A combining mark (Unicode's marks, General Category M, but for the variation selectors that pick an emoji's
/// presentation). A text with one is shaped whole.
fn is_mark(c: char) -> bool {
    use icu_properties::props::{GeneralCategory, GeneralCategoryGroup};
    let u = u32::from(c);
    u >= 0x300
        && !matches!(u, 0xFE00..=0xFE0F | 0xE0100..=0xE01EF)
        && GeneralCategoryGroup::Mark
            .contains(icu_properties::CodePointMapData::<GeneralCategory>::new().get(c))
}

fn key_of(units: &[u16]) -> u64 {
    units
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, &u| (h ^ u64::from(u)).wrapping_mul(0x0100_0000_01b3))
}

impl Shaped {
    /// The text that was shaped.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf16_lossy(&self.units)
    }

    /// Whether this text is `prev` with more after it (words streaming in): each unit of `prev` is the same unit here.
    #[must_use]
    pub fn extends(&self, prev: &Self) -> bool {
        self.units.starts_with(&prev.units)
    }
}

/// What a line the typesetter set showed of the cut at a unit: the line starting or ending there was its paragraph's
/// glyphs cut there, or was not.
struct Seam;

impl Seam {
    const STARTS: u8 = 1;
    const ENDS: u8 = 2;
    const STARTS_NOT: u8 = 4;
    const ENDS_NOT: u8 = 8;
}

/// How many glyphs a line cut at both ends must hold to be cut from its paragraph, or to show anything of its cuts. A
/// contextual lookup looks a few glyphs either way, so in a shorter line one glyph can see past both cuts at once: "H₂"
/// alone sets its subscript wider than "H₂" in "-H₂" or in "H₂O" does, though neither cut alone changes it.
const REACH: usize = 8;

/// A hard paragraph (its newline included) as the typesetter sets it on one line: the glyphs a line broken from it is
/// cut from.
struct Para {
    start: usize,
    end: usize,
    set: Rc<SetLine>,
    glyphs: Vec<Glyph>,
    /// Each glyph's x and advance as the typesetter has them, unrounded.
    xs: Vec<f64>,
    adv: Vec<f64>,
    /// The width the whole paragraph takes ([`Line::width`]), and with its trailing space.
    width: f32,
    full: f64,
    /// Every run is left to right: a line's glyphs are the paragraph's from the line's first to its last.
    ltr: bool,
    /// For each UTF-16 unit from `start` to `end`, both included, the glyph a line starting there starts at: the glyphs
    /// before it are all set from units before it and the rest from units after (no ligature or reordered mark spans the
    /// cut). `u32::MAX` where a glyph does.
    cut: Vec<u32>,
}

/// What a set line holds: its glyphs with each one's unrounded x and advance, whether every run is left to right, and
/// the width the text takes with and without its trailing space (a wrapped line's trailing space hangs past the edge, as
/// in any text view).
#[derive(Clone)]
struct Read {
    glyphs: Vec<Glyph>,
    xs: Vec<f64>,
    adv: Vec<f64>,
    ltr: bool,
    width: f32,
    full: f64,
}

/// A unit that hangs past the end of a line: white space and line ends.
fn hangs(u: u16) -> bool {
    matches!(u, 0x09..=0x0D | 0x20 | 0x85 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000)
}

impl Para {
    fn read(set: Rc<SetLine>, start: usize, end: usize) -> Self {
        let Read { glyphs, xs, adv, ltr, width, full } = set.read();
        let n = glyphs.len();
        let mut cut = vec![u32::MAX; end - start + 1];
        let mut least = vec![usize::MAX; n + 1];
        for k in (0..n).rev() {
            least[k] = least[k + 1].min(glyphs[k].idx);
        }
        let mut most: Option<usize> = None;
        for k in 0..n {
            let at = glyphs[k].idx;
            if (start..end).contains(&at) && most.is_none_or(|m| m < at) && least[k] == at {
                cut[at - start] = k as u32;
            }
            most = Some(most.map_or(at, |m| m.max(at)));
        }
        cut[end - start] = n as u32;
        Self { start, end, set, glyphs, xs, adv, width, full, ltr, cut }
    }
}

impl Shaped {
    /// `text` shaped for [`Layout::wrap`]: as [`Layout::rich_with`] sets it, at any width.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fonts: &mut Fonts,
        text: &str,
        runs: &[Run],
        code: &[(usize, usize)],
        holes: &[Hole],
        style: Style,
        code_line: f32,
    ) -> Self {
        Self::after(fonts, None, text, runs, code, holes, style, code_line)
    }

    /// [`Shaped::new`], keeping each piece `prev` shaped that the text still has as it was (a reply streaming in shapes
    /// only its last paragraph again; a displayed formula's indent moving with the width, only the formula's paragraph).
    #[allow(clippy::too_many_arguments)]
    pub fn after(
        fonts: &mut Fonts,
        prev: Option<&Shaped>,
        text: &str,
        runs: &[Run],
        code: &[(usize, usize)],
        holes: &[Hole],
        style: Style,
        code_line: f32,
    ) -> Self {
        let mut sh = Self::unbroken(fonts, text, runs, code, holes, style, code_line);
        let mut kept: HashMap<u64, Vec<Rc<Piece>>> = HashMap::new();
        if let Some(prev) = prev.filter(|p| {
            (p.style.size, p.style.bold, p.look) == (style.size, style.bold, fonts.look)
        }) {
            for (_, pc) in &prev.pieces {
                kept.entry(pc.key).or_default().push(pc.clone());
            }
        }
        let whole = text.chars().any(is_mark);
        let (mut a, mut at, mut after_separator) = (0usize, 0usize, false);
        for c in text.chars() {
            if after_separator && !separates(c) && !whole {
                let pc = sh.piece(fonts, &kept, a, at);
                sh.pieces.push((a, pc));
                a = at;
            }
            after_separator = separates(c);
            at += c.len_utf16();
        }
        if at > a {
            let pc = sh.piece(fonts, &kept, a, at);
            sh.pieces.push((a, pc));
        }
        sh
    }

    /// What the text's lines are measured from, its pieces not yet shaped.
    #[allow(clippy::too_many_arguments)]
    fn unbroken(
        fonts: &mut Fonts,
        text: &str,
        runs: &[Run],
        code: &[(usize, usize)],
        holes: &[Hole],
        style: Style,
        code_line: f32,
    ) -> Self {
        let font = fonts.ui_font(style.size, style.bold);
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut seen: HashMap<Face, Metrics> = HashMap::new();
        let faces = runs
            .iter()
            .map(|r| {
                *seen
                    .entry(r.face)
                    .or_insert_with(|| Metrics::of(&fonts.face_font(style.size, r.face)))
            })
            .collect();
        let (mut code, mut holes) = (code.to_vec(), holes.to_vec());
        code.sort_unstable();
        holes.sort_by_key(|h| h.start);
        Self {
            units,
            pieces: Vec::new(),
            runs: runs.to_vec(),
            faces,
            base: Metrics::of(&font),
            code,
            items: Vec::new(),
            holes,
            style,
            code_line,
            look: fonts.look,
        }
    }

    /// The text with a line beginning at one of `items` (a list item's start, `Rich::items`) the look's `item` of a line
    /// taller, the room above its words; markdown text only (a code line height asked).
    #[must_use]
    pub fn spaced(mut self, items: &[usize]) -> Self {
        self.items = items.to_vec();
        self.items.sort_unstable();
        self
    }

    /// The runs and holes of the text's units `a..z`, counted from `a`.
    fn within(&self, a: usize, z: usize) -> (Vec<Run>, Vec<Hole>) {
        let runs = self
            .runs
            .iter()
            .filter(|r| r.start < z && r.end > a)
            .map(|r| Run { start: r.start.max(a) - a, end: r.end.min(z) - a, face: r.face })
            .collect();
        let holes = self
            .holes
            .iter()
            .filter(|h| (a..z).contains(&h.start))
            .map(|h| Hole { start: h.start - a, end: h.end.min(z) - a, ..*h })
            .collect();
        (runs, holes)
    }

    /// The units of the text a typesetter for its units `a..z` is made over: [`CONTEXT`] more either side, no pair of
    /// surrogates split.
    fn context(&self, a: usize, z: usize) -> (usize, usize) {
        let low = |u: u16| (0xDC00..0xE000).contains(&u);
        let mut x = a.saturating_sub(CONTEXT);
        while x > 0 && low(self.units[x]) {
            x -= 1;
        }
        let mut y = (z + CONTEXT).min(self.units.len());
        while y < self.units.len() && low(self.units[y]) {
            y += 1;
        }
        (x, y)
    }

    /// The typesetter over `units`, with `runs` and `holes`.
    fn typesetter(
        &self,
        fonts: &mut Fonts,
        units: &[u16],
        runs: &[Run],
        holes: &[Hole],
    ) -> Typesetter {
        let text = String::from_utf16(units).expect("no pair of surrogates is split");
        let font = fonts.ui_font(self.style.size, self.style.bold);
        typesetter(fonts, &text, units.len(), runs, holes, self.style, &font)
    }

    /// The piece over the text's units `a..z`: one of `kept` when it is the same and in the same context, else shaped.
    fn piece(
        &self,
        fonts: &mut Fonts,
        kept: &HashMap<u64, Vec<Rc<Piece>>>,
        a: usize,
        z: usize,
    ) -> Rc<Piece> {
        let (x, y) = self.context(a, z);
        let units = self.units[x..y].to_vec();
        let (runs, holes) = self.within(x, y);
        let (lead, len) = (a - x, z - a);
        let key = key_of(&units) ^ lead as u64;
        // A hole in the context shapes the piece by where it is, not by how wide it is (a displayed formula's indent, which
        // moves with the width, leaves the paragraphs either side as they were).
        let shapes = |h: &Hole| {
            (h.start, h.end, (lead..lead + len).contains(&h.start).then_some(h.width.to_bits()))
        };
        let same = |pc: &&Rc<Piece>| {
            (pc.lead, pc.len) == (lead, len)
                && pc.units == units
                && pc.runs == runs
                && pc.holes.iter().map(shapes).eq(holes.iter().map(shapes))
        };
        if let Some(pc) = kept.get(&key).and_then(|v| v.iter().find(same)) {
            return pc.clone();
        }
        let ts = self.typesetter(fonts, &units, &runs, &holes);
        let mut paras = Vec::new();
        let mut start = lead;
        while start < lead + len {
            let n = ts.suggest_line_break(start, 1.0e7).min(lead + len - start);
            paras.push(Para::read(ts.line(start, n), start, start + n));
            start += n;
        }
        let seams = (0..=units.len()).map(|_| Cell::new(0)).collect();
        Rc::new(Piece {
            units,
            lead,
            len,
            runs,
            holes,
            key,
            ts,
            paras,
            seams,
            broken: RefCell::new(None),
        })
    }

    /// What lines broken `width` wide from the start of the piece at the text's unit `at` (`pc`) are made of beyond the
    /// piece, as a hash: the line heights, the code and holes within it (as far into it as they reach and how tall) and
    /// the bands it is broken in ([`Band`]).
    fn breaks_key(&self, at: usize, pc: &Piece, width: f64, bands: &[Band]) -> u64 {
        let z = at + pc.len;
        let mix = |h: u64, v: u64| (h ^ v).wrapping_mul(0x0100_0000_01b3);
        let mut h = [
            width.to_bits(),
            u64::from(self.style.line.to_bits()),
            u64::from(self.code_line.to_bits()),
        ]
        .into_iter()
        .fold(0xcbf2_9ce4_8422_2325, mix);
        for &(a, b) in self.code.iter().filter(|&&(a, b)| a < z && b > at) {
            h = mix(mix(h, (a.max(at) - at) as u64), (b.min(z) - at) as u64);
        }
        for band in bands.iter().filter(|b| b.lo < z && b.hi > at) {
            h = [
                (band.lo.max(at) - at) as u64,
                (band.hi.min(z) - at) as u64,
                u64::from(band.width.to_bits()),
            ]
            .into_iter()
            .fold(mix(h, 0xb4d), mix);
        }
        let first = self.holes.partition_point(|x| x.start < at);
        for x in self.holes[first..].iter().take_while(|x| x.start < z) {
            h = [
                (x.start - at) as u64,
                u64::from(x.width.to_bits()),
                u64::from(x.ascent.to_bits()),
                u64::from(x.descent.to_bits()),
                u64::from(x.pad.to_bits()),
            ]
            .into_iter()
            .fold(h, mix);
        }
        h
    }

    /// The box of a line starting at the text's unit `s`, its top at `top`: its height, baseline and optical middle. A
    /// list item's room ([`Shaped::spaced`]) is the top of its box, its words set in the rest.
    fn frame(&self, s: usize, top: f32) -> (f32, f32, f32) {
        let height =
            box_height(&self.units, &self.code, self.style, self.code_line, self.look.blank, s);
        let above = if self.code_line > 0.0 && self.items.binary_search(&s).is_ok() {
            (self.style.line * self.look.item).round()
        } else {
            0.0
        };
        let asked = self
            .runs
            .iter()
            .position(|r| (r.start..r.end).contains(&s))
            .map_or(self.base, |i| self.faces[i]);
        let baseline = top + above + asked.baseline(height);
        (height + above, baseline, baseline - asked.rise)
    }

    /// A piece's line as broken before (`l`), `units` further along the text and its top at `top`: its box placed as
    /// [`Shaped::line`] places it.
    fn placed(&self, l: &Line, units: isize, top: f32) -> Line {
        let mut l = l.moved(units);
        l.top = top;
        (l.height, l.baseline, l.mid) = self.frame(l.start, top);
        l.lift(l.holes_in(&self.holes));
        l
    }

    /// Line `[s, e)` of paragraph `p` of the piece `pc`, its top at `top`: `s` and `e` are the piece's indices, which
    /// stand for the text's units `base + s` and `base + e`.
    fn line(&self, base: usize, pc: &Piece, p: &Para, s: usize, e: usize, top: f32) -> Line {
        let at = |i: usize| base + i;
        let (height, baseline, mid) = self.frame(at(s), top);
        let (mut glyphs, width, set) = if (s, e) == (p.start, p.end) {
            (p.glyphs.clone(), p.width, OnceCell::from(p.set.clone()))
        } else {
            let span = pc.span(p, s, e);
            if let Some((a, b)) = span.filter(|&(a, b)| pc.trusted(p, s, e, b - a)) {
                let (glyphs, width) = pc.cut(p, a, b);
                (glyphs, width, OnceCell::new())
            } else {
                let set = pc.ts.line(s, e - s);
                let read = set.read();
                if let Some((a, b)) = span {
                    pc.learn(p, s, e, a, b, &read);
                }
                (read.glyphs, read.width, OnceCell::from(set))
            }
        };
        for g in &mut glyphs {
            g.idx = at(g.idx);
        }
        let mut l = Line {
            start: at(s),
            end: at(e),
            top,
            height,
            baseline,
            mid,
            width,
            glyphs,
            hard_break: pc.units[e - 1] == 10,
            set,
            base,
        };
        l.fit_holes(&self.holes);
        l
    }

    /// The line from the text's unit `s` as the typesetter sets it when it runs past the end of its piece (the text's units
    /// `a..end`): a line wider than the width takes the white space after it and a line end (a formula wider than the
    /// column takes the newlines after it), which may lie past the context the piece was typeset with.
    fn across(
        &self,
        fonts: &mut Fonts,
        a: usize,
        s: usize,
        end: usize,
        width: f64,
        top: f32,
    ) -> Line {
        let len = self.units.len();
        let mut z = end;
        while z < len && hangs(self.units[z]) && !breaks(self.units[z]) {
            z += 1;
        }
        let mut z = (z + 1).min(len);
        loop {
            // From the piece's start: the paragraph's direction is its first strong letter's.
            let (x, y) = self.context(a, z);
            let (runs, holes) = self.within(x, y);
            let ts = self.typesetter(fonts, &self.units[x..y], &runs, &holes);
            let from = s - x;
            let n = ts.suggest_line_break(from, width).min(y - s);
            let e = s + n;
            if e < y || y == len {
                let set = ts.line(from, n);
                let mut read = set.read();
                for g in &mut read.glyphs {
                    g.idx += x;
                }
                let (height, baseline, mid) = self.frame(s, top);
                let mut l = Line {
                    start: s,
                    end: e,
                    top,
                    height,
                    baseline,
                    mid,
                    width: read.width,
                    glyphs: read.glyphs,
                    hard_break: self.units[e - 1] == 10,
                    set: OnceCell::from(set),
                    base: x,
                };
                l.fit_holes(&self.holes);
                return l;
            }
            z = y;
        }
    }
}

impl Piece {
    /// The paragraph's glyphs line `[s, e)` of paragraph `p` would be cut from, when cutting them could be how the typesetter
    /// sets the line: it lies in the paragraph, the paragraph runs left to right, no glyph spans either cut, no tab is
    /// measured from a line start other than the paragraph's (a tab stop is counted from the start of its line), and no
    /// hole is in it (the caret after a kerned space is part-way into the kern, which a hole is measured from).
    fn span(&self, p: &Para, s: usize, e: usize) -> Option<(usize, usize)> {
        if !p.ltr
            || e > p.end
            || (s > p.start && self.units[s..e].contains(&9))
            || self.holes.iter().any(|h| (s..e).contains(&h.start))
        {
            return None;
        }
        let (a, b) = (p.cut[s - p.start], p.cut[e - p.start]);
        (a != u32::MAX && b != u32::MAX).then_some((a as usize, b as usize))
    }

    /// Whether the typesetter has shown that it sets a line starting at `s` and one ending at `e` as their paragraph's glyphs cut
    /// there (a paragraph's own ends are no cut), for a line of `glyphs` glyphs.
    fn trusted(&self, p: &Para, s: usize, e: usize, glyphs: usize) -> bool {
        (s == p.start || e == p.end || glyphs >= REACH)
            && (s == p.start
                || self.seams[s].get() & (Seam::STARTS | Seam::STARTS_NOT) == Seam::STARTS)
            && (e == p.end || self.seams[e].get() & (Seam::ENDS | Seam::ENDS_NOT) == Seam::ENDS)
    }

    /// Remember what line `[s, e)` as the typesetter set it (`read`) shows of its cuts: its paragraph's glyphs `a..b` cut there
    /// are the same glyphs at the same places and as wide, to the bit, or are not. A cut is shown only by a line that holds
    /// every glyph it could change: one running to its paragraph's other end, or [`REACH`] glyphs long. When they differ,
    /// the cuts not yet shown sound are to blame, or both when both were.
    fn learn(&self, p: &Para, s: usize, e: usize, a: usize, b: usize, read: &Read) {
        let long = b - a >= REACH;
        let (shows_head, shows_tail) =
            (s > p.start && (long || e == p.end), e < p.end && (long || s == p.start));
        if !shows_head && !shows_tail {
            return;
        }
        let (glyphs, width) = self.cut(p, a, b);
        let key = |g: &Glyph| (g.font, g.id, g.idx, g.x.to_bits(), g.y.to_bits());
        let same = width.to_bits() == read.width.to_bits()
            && glyphs.len() == read.glyphs.len()
            && glyphs.iter().zip(&read.glyphs).all(|(g, h)| key(g) == key(h));
        let (head, tail) = (&self.seams[s], &self.seams[e]);
        if same {
            if shows_head {
                head.set(head.get() | Seam::STARTS);
            }
            if shows_tail {
                tail.set(tail.get() | Seam::ENDS);
            }
            return;
        }
        let (head_shown, tail_shown) = (
            !shows_head || head.get() & Seam::STARTS != 0,
            !shows_tail || tail.get() & Seam::ENDS != 0,
        );
        let both = head_shown && tail_shown;
        if shows_head && (both || !head_shown) {
            head.set(head.get() | Seam::STARTS_NOT);
        }
        if shows_tail && (both || !tail_shown) {
            tail.set(tail.get() | Seam::ENDS_NOT);
        }
    }

    /// Glyphs `a..b` of paragraph `p` as a line of their own, and the width they take.
    fn cut(&self, p: &Para, a: usize, b: usize) -> (Vec<Glyph>, f32) {
        let x0 = p.xs.get(a).copied().unwrap_or(0.0);
        let glyphs = p.glyphs[a..b]
            .iter()
            .zip(&p.xs[a..b])
            .map(|(g, &x)| Glyph { x: (x - x0) as f32, ..*g })
            .collect();
        let full: f64 = p.adv[a..b].iter().sum();
        let trailing: f64 = (a..b)
            .rev()
            .take_while(|&k| hangs(self.units[p.glyphs[k].idx]))
            .map(|k| p.adv[k])
            .sum();
        (glyphs, (full - trailing) as f32)
    }
}

impl Layout {
    /// The first line's optical middle ([`Line::mid`]) below the layout's top: set it on a row's centre and the words
    /// sit level with an icon centred there.
    #[must_use]
    pub fn mid(&self) -> f32 {
        self.lines.first().map_or(self.line_h * 0.5, |l| l.mid)
    }

    /// Shape and break `text` to `max_w` points (0 or less: no wrapping beyond hard newlines).
    pub fn new(fonts: &mut Fonts, text: &str, style: Style, max_w: f32) -> Self {
        Self::rich(fonts, text, &[], &[], style, 0.0, max_w)
    }

    /// [`Layout::new`] with the whole text set in `face`.
    pub fn in_face(fonts: &mut Fonts, text: &str, face: Face, style: Style, max_w: f32) -> Self {
        let len = text.encode_utf16().count();
        Self::rich(fonts, text, &[Run { start: 0, end: len, face }], &[], style, 0.0, max_w)
    }

    /// [`Layout::new`] with `runs` set in their faces. A line starting inside one of the UTF-16 ranges `code` is
    /// `code_line` points tall (when positive) instead of the style's; a line that wraps inside one is code too.
    pub fn rich(
        fonts: &mut Fonts,
        text: &str,
        runs: &[Run],
        code: &[(usize, usize)],
        style: Style,
        code_line: f32,
        max_w: f32,
    ) -> Self {
        Self::rich_with(fonts, text, runs, code, &[], style, code_line, max_w)
    }

    /// [`Layout::rich`] with room left for each of `holes`: where a hole stands in the text is a no-break space made as wide
    /// as the hole, and a line that holds one is as tall as the hole needs.
    #[allow(clippy::too_many_arguments)]
    pub fn rich_with(
        fonts: &mut Fonts,
        text: &str,
        runs: &[Run],
        code: &[(usize, usize)],
        holes: &[Hole],
        style: Style,
        code_line: f32,
        max_w: f32,
    ) -> Self {
        let shaped = Rc::new(Shaped::new(fonts, text, runs, code, holes, style, code_line));
        Self::wrap(fonts, &shaped, max_w)
    }

    /// `shaped` broken to `max_w` points (0 or less: no wrapping beyond hard newlines). The typesetter picks every
    /// break; a paragraph that fits is its own line, and a line is cut from its paragraph's glyphs where the typesetter has
    /// shown that cutting there is what it sets, else the typesetter sets it (and so shows what its cuts do).
    pub fn wrap(fonts: &mut Fonts, shaped: &Rc<Shaped>, max_w: f32) -> Self {
        Self::wrap_with(fonts, shaped, max_w, &[])
    }

    /// [`Layout::wrap`] with the lines that start in each of `bands` broken to the band's own width.
    pub fn wrap_with(fonts: &mut Fonts, shaped: &Rc<Shaped>, max_w: f32, bands: &[Band]) -> Self {
        let sh = &**shaped;
        let outer = if max_w > 0.0 { f64::from(max_w) } else { 1.0e7 };
        let len = sh.units.len();
        let mut lines: Vec<Line> = Vec::new();
        let (mut top, mut start, mut k) = (0.0f32, 0usize, 0usize);
        while start < len {
            // A line the typesetter ends past its piece moves the next line's start into a later one.
            while sh.pieces[k].0 + sh.pieces[k].1.len <= start {
                k += 1;
            }
            let (at, pc) = (sh.pieces[k].0, &*sh.pieces[k].1);
            let key = (start == at).then(|| sh.breaks_key(at, pc, outer, bands));
            if let Some(b) = pc.broken.borrow().as_ref().filter(|b| Some(b.key) == key) {
                for l in &b.lines {
                    let l = sh.placed(l, at as isize, top);
                    top += l.height;
                    lines.push(l);
                }
                start = at + pc.len;
                continue;
            }
            let base = at - pc.lead;
            let (first, mut pi, mut crossed) = (lines.len(), 0, false);
            while start < at + pc.len {
                let s = start - base;
                while pc.paras[pi].end <= s {
                    pi += 1;
                }
                let p = &pc.paras[pi];
                let width = band_width(bands, start, outer);
                let e = if s == p.start && p.full <= width - FITS {
                    p.end
                } else {
                    (s + pc.ts.suggest_line_break(s, width)).min(pc.units.len())
                };
                crossed |= e > pc.lead + pc.len;
                let l = if e > pc.lead + pc.len {
                    sh.across(fonts, at, start, at + pc.len, width, top)
                } else {
                    sh.line(base, pc, p, s, e, top)
                };
                top += l.height;
                start = l.end;
                lines.push(l);
            }
            if let Some(key) = key.filter(|_| !crossed) {
                let lines = lines[first..].iter().map(|l| l.moved(-(at as isize))).collect();
                *pc.broken.borrow_mut() = Some(Broken { key, lines });
            }
        }
        if len == 0 || sh.units.last() == Some(&10) {
            let top = lines.last().map_or(0.0, |l| l.top + l.height);
            let baseline = top + sh.base.baseline(sh.style.line);
            lines.push(Line {
                start: len,
                end: len,
                top,
                height: sh.style.line,
                baseline,
                mid: baseline - sh.base.rise,
                width: 0.0,
                glyphs: Vec::new(),
                hard_break: false,
                set: OnceCell::new(),
                base: 0,
            });
        }
        Self {
            lines,
            line_h: sh.style.line,
            code_line: sh.code_line,
            len,
            size: sh.style.size,
            tail: 0.0,
            sliver: sh.look.sliver,
            holes: sh.holes.clone(),
            shaped: shaped.clone(),
            wrap_w: max_w,
            bands: bands.to_vec(),
            slides: RefCell::default(),
        }
    }

    /// What the layout is made of, to break it again the way it was broken: the shaped text and the width it was wrapped to.
    #[must_use]
    pub fn source(&self) -> (Rc<Shaped>, f32) {
        (self.shaped.clone(), self.wrap_w)
    }

    /// The bands it was broken in ([`Layout::wrap_with`]).
    #[must_use]
    pub fn bands(&self) -> &[Band] {
        &self.bands
    }

    /// The text broken again as this layout was (its width and bands), with its slides: what a layout drawn this frame
    /// is, made again from what was kept of it.
    pub fn again(
        fonts: &mut Fonts,
        shaped: &Rc<Shaped>,
        max_w: f32,
        bands: &[Band],
        slides: Vec<Slide>,
    ) -> Self {
        let lay = Self::wrap_with(fonts, shaped, max_w, bands);
        lay.set_slides(slides);
        lay
    }

    /// Move and cut runs of its lines ([`Slide`]), in place of the ones it had: a layout kept in a cache is the one the
    /// transcript draws, so its code blocks scroll without the text being broken again.
    pub fn set_slides(&self, slides: Vec<Slide>) {
        *self.slides.borrow_mut() = slides;
    }

    /// Its slides.
    #[must_use]
    pub fn slides(&self) -> Vec<Slide> {
        self.slides.borrow().clone()
    }

    /// The slide line `li` is in, if one.
    #[must_use]
    pub fn slide(&self, li: usize) -> Option<Slide> {
        let s = self.lines.get(li)?.start;
        self.slides.borrow().iter().find(|sl| (sl.lo..sl.hi).contains(&s)).copied()
    }

    /// How far line `li` is moved along ([`Slide::dx`]; 0 for a line in none).
    #[must_use]
    pub fn dx(&self, li: usize) -> f32 {
        self.slide(li).map_or(0.0, |s| s.dx)
    }

    /// The layout's words, as set (a hole's stand-in space included).
    #[must_use]
    pub fn text(&self) -> String {
        self.shaped.text()
    }

    /// The typesetter's line for line `li` (none for the empty line after a final newline), made on first use from the
    /// typesetter of the piece it is in.
    fn set_line(&self, li: usize) -> Option<&SetLine> {
        let l = &self.lines[li];
        if l.start == l.end {
            return None;
        }
        if let Some(set) = l.set.get() {
            return Some(set);
        }
        let (at, pc) = &self.shaped.pieces
            [self.shaped.pieces.partition_point(|(at, _)| *at <= l.start).checked_sub(1)?];
        Some(&**l.set.get_or_init(|| pc.ts.line(l.start + pc.lead - at, l.end - l.start)))
    }

    /// The layout as the typesetter alone sets it, typesetting the text and every line afresh: what [`Layout::wrap`] must match.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn typeset(
        fonts: &mut Fonts,
        text: &str,
        runs: &[Run],
        code: &[(usize, usize)],
        holes: &[Hole],
        style: Style,
        code_line: f32,
        max_w: f32,
    ) -> Self {
        Self::typeset_with(fonts, text, runs, code, holes, style, code_line, max_w, &[])
    }

    /// [`Layout::typeset`] with `bands`: what [`Layout::wrap_with`] must match.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn typeset_with(
        fonts: &mut Fonts,
        text: &str,
        runs: &[Run],
        code: &[(usize, usize)],
        holes: &[Hole],
        style: Style,
        code_line: f32,
        max_w: f32,
        bands: &[Band],
    ) -> Self {
        let font = fonts.ui_font(style.size, style.bold);
        let len: usize = text.encode_utf16().count();
        let mut lines = Vec::new();
        let shaped = Rc::new(Shaped::unbroken(fonts, text, runs, code, holes, style, code_line));
        if len > 0 {
            let units: Vec<u16> = text.encode_utf16().collect();
            let ts = typesetter(fonts, text, len, runs, holes, style, &font);
            let width = if max_w > 0.0 { f64::from(max_w) } else { 1.0e7 };
            let mut start = 0usize;
            let mut top = 0.0f32;
            while start < len {
                let n =
                    ts.suggest_line_break(start, band_width(bands, start, width)).min(len - start);
                let line = ts.line(start, n);
                let height = box_height(&units, code, style, code_line, fonts.look.blank, start);
                let asked = runs
                    .iter()
                    .find(|r| (r.start..r.end).contains(&start))
                    .map_or_else(|| font.clone(), |r| fonts.face_font(style.size, r.face));
                let mut l = make_line(line, &asked, start, start + n, top, height);
                l.hard_break = text.encode_utf16().nth(start + n - 1) == Some(10);
                l.fit_holes(&shaped.holes);
                top += l.height;
                lines.push(l);
                start += n;
            }
        }
        let ends_nl = text.ends_with('\n');
        if len == 0 || ends_nl {
            let top = lines.last().map_or(0.0, |l| l.top + l.height);
            let baseline = top + Metrics::of(&font).baseline(style.line);
            lines.push(Line {
                start: len,
                end: len,
                top,
                height: style.line,
                baseline,
                mid: baseline - optical_rise(&font),
                width: 0.0,
                glyphs: Vec::new(),
                hard_break: false,
                set: OnceCell::new(),
                base: 0,
            });
        }
        Self {
            lines,
            line_h: style.line,
            code_line,
            len,
            size: style.size,
            tail: 0.0,
            sliver: fonts.look.sliver,
            holes: shaped.holes.clone(),
            shaped,
            wrap_w: max_w,
            bands: bands.to_vec(),
            slides: RefCell::default(),
        }
    }

    /// Make room for `h` points under line `li`: the lines after it move down, and the room is the gap between.
    pub fn open_gap(&mut self, li: usize, h: f32) {
        if li + 1 >= self.lines.len() {
            self.tail += h;
            return;
        }
        for l in &mut self.lines[li + 1..] {
            l.top += h;
            l.baseline += h;
        }
    }

    /// Make room for `h` points over line `li`: it and the lines after it move down (the first line too, unlike [`Self::open_gap`]).
    pub fn open_room_above(&mut self, li: usize, h: f32) {
        if li >= self.lines.len() {
            self.tail += h;
            return;
        }
        for l in &mut self.lines[li..] {
            l.top += h;
            l.baseline += h;
        }
    }

    #[must_use]
    pub fn height(&self) -> f32 {
        self.lines.last().map_or(0.0, |l| l.top + l.height) + self.tail
    }

    /// How wide its lines reach: a slid line's from its move to where it is cut.
    #[must_use]
    pub fn width(&self) -> f32 {
        let slides = self.slides.borrow();
        let reach = |l: &Line| match slides.iter().find(|s| (s.lo..s.hi).contains(&l.start)) {
            Some(s) => (s.dx + l.width).min(s.clip[1]).max(0.0),
            None => l.width,
        };
        self.lines.iter().map(reach).fold(0.0, f32::max)
    }

    /// Index of the line a caret at UTF-16 `idx` sits on (a wrap point belongs to the line that starts there).
    #[must_use]
    pub fn line_of(&self, idx: usize) -> usize {
        // The lines cover the text in order, each from where the last ended; only an empty last line has no units.
        if idx >= self.len {
            return self.lines.len().saturating_sub(1);
        }
        self.lines.partition_point(|l| l.end <= idx).min(self.lines.len().saturating_sub(1))
    }

    /// The lines whose boxes reach into the band from `y0` to `y1` below the layout's top.
    #[must_use]
    pub fn rows_within(&self, y0: f32, y1: f32) -> std::ops::Range<usize> {
        let first = self.lines.partition_point(|l| l.top + l.height < y0);
        first..self.lines.partition_point(|l| l.top <= y1).max(first)
    }

    /// Caret position: x from the layout origin and the line index.
    #[must_use]
    pub fn caret(&self, idx: usize) -> (f32, usize) {
        let li = self.line_of(idx);
        (self.offset(li, idx), li)
    }

    /// Where a caret at `idx` sits on line `li`, moved with the line's slide.
    fn offset(&self, li: usize, idx: usize) -> f32 {
        self.dx(li) + self.unmoved(li, idx)
    }

    fn unmoved(&self, li: usize, idx: usize) -> f32 {
        let l = &self.lines[li];
        if let Some(h) =
            self.holes.iter().find(|h| h.end == idx && h.start >= l.start && h.start < l.end)
        {
            return self.unmoved(li, h.start) + h.width;
        }
        match self.set_line(li) {
            Some(set) => set.offset(idx.clamp(l.start, l.end) - l.base) as f32,
            None => 0.0,
        }
    }

    /// UTF-16 index nearest to a point (layout origin).
    #[must_use]
    pub fn index_at(&self, x: f32, y: f32) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let li = self.lines.partition_point(|l| l.top + l.height <= y).min(self.lines.len() - 1);
        let l = &self.lines[li];
        let Some(set) = self.set_line(li) else { return l.start };
        // A slid line answers only where it shows: a point past its cut is at the edge.
        let x = self.slide(li).map_or(x, |s| x.clamp(s.clip[0], s.clip[1].max(s.clip[0])) - s.dx);
        let i = set.index_at(f64::from(x)).map_or(l.start, |i| i + l.base);
        i.clamp(l.start, if l.hard_break { l.end - 1 } else { l.end })
    }
}

/// How far over the baseline the middle of a font's letters is to the eye: the mean of half its cap height and half
/// its x-height.
fn optical_rise(font: &Font) -> f32 {
    (font.cap_height() + font.x_height()) * 0.25
}

/// The set line `set` as a [`Line`], its baseline and optical middle from `font` ([`Metrics`]).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn make_line(
    set: Rc<SetLine>,
    font: &Font,
    start: usize,
    end: usize,
    top: f32,
    height: f32,
) -> Line {
    let Read { glyphs, width, .. } = set.read();
    let baseline = top + Metrics::of(font).baseline(height);
    let mid = baseline - optical_rise(font);
    Line {
        start,
        end,
        top,
        height,
        baseline,
        mid,
        width,
        glyphs,
        hard_break: false,
        set: OnceCell::from(set),
        base: 0,
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    /// A line as the eye and the caret see it: its units, box, width and every glyph's face, id, unit and place, to the bit.
    fn seen(l: &Line) -> (usize, usize, [u32; 4], bool, Vec<(FontId, u16, usize, u32, u32)>) {
        let boxed = [l.top, l.height, l.baseline, l.width].map(f32::to_bits);
        let glyphs = l.glyphs.iter().map(|g| (g.font, g.id, g.idx, g.x.to_bits(), g.y.to_bits())).collect();
        (l.start, l.end, boxed, l.hard_break, glyphs)
    }

    /// Prose as a transcript holds it: runs of words, a bold run, a long unbreakable token, curly quotes, a hard line end
    /// and a blank line.
    const PROSE: &str = "Live re-wrap keeps every word where the typesetter puts it, whatever width came before: \
                         blaze builds blaze, and the harness's transcript follows its column as the sidebar folds.\n\
                         A second paragraph, with a long-unbroken-identifier_like_this_one and \u{201c}quotes\u{201d}, \
                         then a line that ends hard.\n\nAfter a blank line, the last words.";
    const BOLD: [Run; 1] = [Run { start: 5, end: 12, face: Face::Bold }];
    const STYLE: Style = Style { size: 14.0, bold: false, line: 20.0 };

    /// The widths a sidebar's spring passes through a fold, narrowing and then widening.
    fn fold_widths() -> impl Iterator<Item = f32> {
        let narrowing = (0..60).map(|k| 640.0 - 7.25 * k as f32);
        let widening = (0..60).map(|k| 212.5 + 7.25 * k as f32);
        narrowing.chain(widening)
    }

    /// A transcript's prose re-wrapping live through a fold, shaped once: broken again at every width a sidebar's spring
    /// passes, narrowing and then widening, each break-only layout ([`Layout::wrap`], reusing the lines, cuts and seams the
    /// widths before it left) is the layout the typesetter sets from nothing at that width ([`Layout::typeset`]), line for
    /// line and glyph for glyph.
    #[test]
    fn breaking_again_at_a_width_is_a_full_layout_at_that_width() {
        let len = PROSE.encode_utf16().count();
        let mut fonts = Fonts::default();
        let shaped = Rc::new(Shaped::new(&mut fonts, PROSE, &BOLD, &[], &[], STYLE, 0.0));
        for w in fold_widths() {
            let again = Layout::wrap(&mut fonts, &shaped, w);
            let full = Layout::typeset(&mut fonts, PROSE, &BOLD, &[], &[], STYLE, 0.0, w);
            assert_eq!(again.lines.len(), full.lines.len(), "at {w} points: {} lines broken again, {} set", again.lines.len(), full.lines.len());
            for (k, (a, b)) in again.lines.iter().zip(&full.lines).enumerate() {
                assert_eq!(seen(a), seen(b), "at {w} points, line {k} differs");
            }
            assert_eq!(again.lines.last().map(|l| l.end), Some(len), "at {w} points the lines do not cover the text");
        }
    }

    /// What one frame of a fold costs a transcript of `HARNESS_BENCH_WRAP` messages (den's `bench_resize`): each width
    /// broken again over text shaped once ([`Layout::wrap`]) against typeset from nothing ([`Layout::typeset`]), printed;
    /// it asserts nothing and is skipped unless the variable is set (run it with `--nocapture` to see the numbers).
    #[test]
    fn fold_frame_cost() {
        let Some(n) = std::env::var("HARNESS_BENCH_WRAP").ok().and_then(|v| v.parse::<usize>().ok()) else { return };
        let mut fonts = Fonts::default();
        let widths: Vec<f32> = fold_widths().collect();
        let per_width = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e3 / widths.len() as f64;
        let t = std::time::Instant::now();
        let shaped: Vec<Rc<Shaped>> =
            (0..n).map(|_| Rc::new(Shaped::new(&mut fonts, PROSE, &BOLD, &[], &[], STYLE, 0.0))).collect();
        let shape = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        for &w in &widths {
            for sh in &shaped {
                std::hint::black_box(Layout::wrap(&mut fonts, sh, w));
            }
        }
        let first = per_width(t);
        let t = std::time::Instant::now();
        for &w in &widths {
            for sh in &shaped {
                std::hint::black_box(Layout::wrap(&mut fonts, sh, w));
            }
        }
        let again = per_width(t);
        let t = std::time::Instant::now();
        for &w in &widths {
            for _ in 0..n {
                std::hint::black_box(Layout::typeset(&mut fonts, PROSE, &BOLD, &[], &[], STYLE, 0.0, w));
            }
        }
        let typeset = per_width(t);
        eprintln!("{n} messages, {} widths: shape once {shape:.2} ms", widths.len());
        eprintln!("wrap, first fold (learning cuts): {first:.3} ms per frame; every fold after: {again:.3} ms per frame");
        eprintln!("typeset afresh: {typeset:.3} ms per frame ({:.1}x the wrap)", typeset / again.max(1e-9));
    }
}

// ---- glyph atlas ---------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct AtlasGlyph {
    /// Atlas texel rectangle.
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Offset of the bitmap's left edge from the glyph origin, device pixels (after the subpixel shift).
    pub left: i32,
    /// Offset of the bitmap's top edge above the baseline, device pixels.
    pub top: i32,
    /// A colour bitmap (an emoji from Apple Color Emoji): drawn as it is, not tinted with the text colour.
    pub colour: bool,
}

/// A hasher for the atlas's keys, which are looked up for every glyph of every frame: each word folded in with one
/// multiply (the keys are fonts, glyphs and sizes, not anybody's input, so they need no protection from collisions).
#[derive(Default)]
struct Quick(u64);

impl std::hash::Hasher for Quick {
    fn finish(&self) -> u64 {
        // The multiply leaves its best bits at the top; the table picks a slot by the bottom ones.
        self.0 ^ (self.0 >> 32)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u16(&mut self, n: u16) {
        self.write_u64(u64::from(n));
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7cc1_b727_220a_95);
    }
}

pub struct GlyphCache {
    pub size: u32,
    packer: Packer,
    map: HashMap<(FontId, u16, u32, u32), Option<AtlasGlyph>, std::hash::BuildHasherDefault<Quick>>,
    /// A glyph found no room since the last [`GlyphCache::clear`]: it was left out, not remembered as missing.
    full: bool,
    /// Lookups found in the map and rasterised anew, since launch (F3's overlay, src/app/stats.rs, reads their rate).
    pub hits: u64,
    pub misses: u64,
}

impl GlyphCache {
    #[must_use]
    pub fn new(size: u32) -> Self {
        Self {
            size,
            packer: Packer::new(size, size),
            map: HashMap::default(),
            full: false,
            hits: 0,
            misses: 0,
        }
    }

    /// Whether a glyph was left out for want of room since the last [`GlyphCache::clear`].
    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.full
    }

    /// How full the atlas is (0 to 1).
    #[must_use]
    pub fn fill(&self) -> f32 {
        self.packer.used()
    }

    /// Forget every glyph and start the atlas over; entries handed out before are no longer valid.
    pub fn clear(&mut self) {
        self.packer = Packer::new(self.size, self.size);
        self.map.clear();
        self.full = false;
    }

    /// The atlas entry for a glyph at `scale` and subpixel step `sub` (0..SUBPX); rasterises on first use and calls
    /// `upload(x, y, w, h, bytes)` with the new premultiplied RGBA texels (a plain glyph's coverage as white).
    pub fn get(
        &mut self,
        fonts: &Fonts,
        font: FontId,
        glyph: u16,
        scale: f32,
        sub: u32,
        upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
    ) -> Option<AtlasGlyph> {
        let key = (font, glyph, scale.to_bits(), sub);
        if let Some(e) = self.map.get(&key) {
            self.hits += 1;
            return *e;
        }
        self.misses += 1;
        let e = self.place(
            font::raster_glyph(fonts.font(font)?, glyph, scale, f64::from(sub) / f64::from(SUBPX)),
            upload,
        );
        if e.is_some() || !self.full {
            self.map.insert(key, e);
        }
        e
    }

    /// The atlas entry for a formula's outline (a radical, a tall bracket, a stretchy arrow: `segs`, in ems from the
    /// outline's origin, y down) at `scale` pixels to the em and subpixel step `sub` across, filled or stroked `stroke` ems
    /// wide; `key` names the shape (math.rs `Item::Path`). The entry's `left` and `top` place it from the origin like a
    /// glyph's. Rasterised on first use, like [`GlyphCache::get`].
    #[allow(clippy::too_many_arguments)]
    pub fn outline(
        &mut self,
        key: u64,
        segs: &[PathSeg],
        fill: bool,
        stroke: f32,
        scale: f32,
        sub: u32,
        upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
    ) -> Option<AtlasGlyph> {
        // Outlines live beside glyphs under a key no glyph has: glyph u16::MAX and bit 29 of the step (a plain glyph's step
        // is below SUBPX, a placed glyph's has bit 31 set), the shape, its stroke and whether it is filled all in it.
        let id = (key ^ u64::from(stroke.to_bits()) << 32 ^ 0x7a3d_51c9, u16::MAX, scale.to_bits(), sub | 0x2000_0000 | u32::from(fill) << 28);
        if let Some(e) = self.map.get(&id) {
            self.hits += 1;
            return *e;
        }
        self.misses += 1;
        let e = self.place(font::raster_outline(segs, fill, stroke, scale, f64::from(sub) / f64::from(SUBPX)), upload);
        if e.is_some() || !self.full {
            self.map.insert(id, e);
        }
        e
    }

    /// The atlas entry for a coverage mask drawn elsewhere (an icon), `w` by `h` pixels under `key`; uploaded on first
    /// use as white, so it tints like a plain glyph.
    pub fn mask(
        &mut self,
        key: (FontId, u16, u32, u32),
        w: u32,
        h: u32,
        alpha: &[u8],
        upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
    ) -> Option<AtlasGlyph> {
        if let Some(e) = self.map.get(&key) {
            return *e;
        }
        let Some((x, y)) = self.packer.alloc(w, h) else {
            self.full = true;
            return None;
        };
        let rgba: Vec<u8> = alpha.iter().flat_map(|&c| [c; 4]).collect();
        upload(x, y, w, h, &rgba);
        let e = Some(AtlasGlyph { x, y, w, h, left: 0, top: 0, colour: false });
        self.map.insert(key, e);
        e
    }

    /// The atlas entry for `glyph` of `font` drawn scaled `sx` by `sy` pixels a point with its origin at (`tx`, `ty`)
    /// pixels from a cell's bottom-left corner (y up): a glyph constrained into a terminal cell
    /// ([`nerd_rules::Constraint`]). Its `left` and `top` are from that corner.
    #[allow(clippy::too_many_arguments)]
    pub fn get_placed(
        &mut self,
        fonts: &Fonts,
        font: FontId,
        glyph: u16,
        sx: f64,
        sy: f64,
        tx: f64,
        ty: f64,
        upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
    ) -> Option<AtlasGlyph> {
        let mut h: u64 = 0x9e37_79b9_7f4a_7c15;
        for v in [sx, sy, tx, ty] {
            h = (h.rotate_left(13) ^ v.to_bits()).wrapping_mul(0x0100_0000_01b3);
        }
        // Placed glyphs live beside plain ones under a key no plain glyph has (a plain key's last field is below SUBPX).
        let key = (font ^ 0x5bd1_e995_0000_0000, glyph, (h >> 32) as u32, (h as u32) | 0x8000_0000);
        if let Some(e) = self.map.get(&key) {
            return *e;
        }
        let f = fonts.font(font)?;
        let [bx, by, bw, bh] = f.bounds(glyph);
        let b = if bw <= 0.0 || bh <= 0.0 {
            font::Bitmap { left: 0, top: 0, w: 0, h: 0, colour: false, rgba: Vec::new() }
        } else {
            let colour = f.colour();
            let x0 = (tx + bx * sx).floor() as i32 - 1;
            let y0 = (ty + by * sy).floor() as i32 - 1;
            let x1 = (tx + (bx + bw) * sx).ceil() as i32 + 1;
            let y1 = (ty + (by + bh) * sy).ceil() as i32 + 1;
            let (w, hh) = ((x1 - x0) as usize, (y1 - y0) as usize);
            let mut buf = vec![0u8; w * hh * if colour { 4 } else { 1 }];
            f.draw_placed(glyph, &mut buf, w, hh, x0, y0, sx, sy, tx, ty);
            let rgba = if colour { buf } else { buf.iter().flat_map(|&c| [c; 4]).collect() };
            font::Bitmap { left: x0, top: y1, w: w as u32, h: hh as u32, colour, rgba }
        };
        let e = self.place(b, upload);
        if e.is_some() || !self.full {
            self.map.insert(key, e);
        }
        e
    }

    /// A bitmap given room in the atlas and uploaded there; one that draws nothing takes no room.
    fn place(
        &mut self,
        b: font::Bitmap,
        upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
    ) -> Option<AtlasGlyph> {
        if b.w == 0 || b.h == 0 {
            return Some(AtlasGlyph { x: 0, y: 0, w: 0, h: 0, left: 0, top: 0, colour: false });
        }
        let Some((x, y)) = self.packer.alloc(b.w, b.h) else {
            self.full = true;
            return None;
        };
        upload(x, y, b.w, b.h, &b.rgba);
        Some(AtlasGlyph { x, y, w: b.w, h: b.h, left: b.left, top: b.top, colour: b.colour })
    }
}

#[cfg(test)]
mod math_tests {
    use super::*;

    /// A bundled KaTeX font loads once and has its letters (and not what it lacks); an outline rasterises into the atlas
    /// solid inside, standing on its origin, and is remembered.
    #[test]
    fn a_math_font_loads_and_an_outline_rasterises_with_coverage() {
        let mut f = Fonts::default();
        let id = f.math_font(Katex::MainRegular).expect("the bundled font loads");
        assert!(f.math_glyph(id, 'x').is_some() && f.math_glyph(id, '\u{4e2d}').is_none(), "it has x and has no Han");
        assert_eq!(f.math_font(Katex::MainRegular), Some(id), "loaded once");
        let mut cache = GlyphCache::new(256);
        let square = [PathSeg::Move([0.0, -1.0]), PathSeg::Line([1.0, -1.0]), PathSeg::Line([1.0, 0.0]), PathSeg::Line([0.0, 0.0]), PathSeg::Close];
        let mut got = (0, 0, 0u8);
        let e = cache.outline(7, &square, true, 0.0, 10.0, 0, &mut |_, _, w, h, b| got = (w, h, b.iter().copied().max().unwrap_or(0))).unwrap();
        assert!(e.w >= 10 && e.h >= 10 && e.top >= 10, "a ten-pixel square standing on its origin: {e:?}");
        assert_eq!(got.2, 255, "solid inside");
        assert!(cache.outline(7, &square, true, 0.0, 10.0, 0, &mut |_, _, _, _, _| panic!("remembered")).is_some());
    }
}
