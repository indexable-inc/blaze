//! LaTeX math, typeset natively. RaTeX (a pure-Rust port of KaTeX's parser and TeX's layout rules, with the metrics of the
//! Computer Modern fonts KaTeX ships) turns the source into a flat list of positioned glyphs, rules and outlines in ems;
//! the painter draws them itself (paint.rs `emit_math`) with its glyph atlas and panels, so a formula is crisp at any zoom
//! and needs no web view. Nothing here touches the screen or a font: a [`Typeset`] is plain numbers, and the same source
//! always gives the same one. Ported from den-native's math.rs (docs/math.md there has the choice of RaTeX).
//!
//! Where formulas are found in the words is markdown.rs (`$…$`, `\(…\)` inline, `$$…$$`, `\[…\]` displayed); a chat's
//! displayed formula on lines of its own, or in a ```math fence, is a row of its own (harness-native's chat.rs).
use ratex_font::katex_ttf_glyph_char;
use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parse;
use ratex_types::{Color, DisplayItem, MathStyle, PathCommand};
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// One of the KaTeX fonts a formula's glyphs are drawn from (their files are in `fonts/katex`).
pub use ratex_font::FontId as Katex;

/// The most items a formula may typeset to: a pasted wall of symbols is shown as source rather than drawn.
const MAX_ITEMS: usize = 4000;

/// How many formulas are remembered, each by its source and whether it is displayed; the one used longest ago goes first.
const MEMO_CAP: usize = 256;

/// A step of an outline, in ems from the outline's own origin, y down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathSeg {
    Move([f32; 2]),
    Line([f32; 2]),
    Quad([f32; 2], [f32; 2]),
    Cubic([f32; 2], [f32; 2], [f32; 2]),
    Close,
}

/// One mark of a formula, in ems from the formula's top-left corner, y down. `ink` is a colour the formula chose for
/// itself (`\color{red}`); none means the ink of the text it stands in.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// The glyph of `ch` in `font`, its baseline origin at `at`, `em` times the formula's size (scripts are smaller).
    Glyph { font: Katex, ch: char, at: [f32; 2], em: f32, ink: Option<[f32; 4]> },
    /// A filled rectangle `[x, y, w, h]`: a fraction bar, a rule, a box's ground.
    Rule { rect: [f32; 4], ink: Option<[f32; 4]> },
    /// An outline at `at` (a radical, a tall delimiter, a stretchy arrow), filled or stroked; `key` names its shape.
    Path { at: [f32; 2], segs: Vec<PathSeg>, fill: bool, ink: Option<[f32; 4]>, key: u64 },
}

/// A formula laid out: how wide it is, and how far it reaches above and below its baseline, in ems.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Typeset {
    pub width: f32,
    pub height: f32,
    pub depth: f32,
    pub items: Vec<Item>,
}

/// The formulas typeset so far: for text style and for display style, each source's result and when it was last asked for.
#[derive(Default)]
struct Memo {
    now: u64,
    styles: [HashMap<String, (Result<Arc<Typeset>, String>, u64)>; 2],
}

thread_local! {
    // Per thread, as the painter is (it lives on the thread that draws); borrowed only inside `typeset`, never across a
    // wait.
    static MEMO: RefCell<Memo> = RefCell::new(Memo::default());
}

/// `tex` typeset: in display style (big operators with limits above and below, fractions at full size) or in text style
/// (what stands in a line of words). The error is the typesetter's own words for what it could not read. Remembered by
/// source and style, so a reply that streams in is not typeset again for every token.
///
/// # Errors
/// The source did not parse, or was empty, or came out too large to draw.
pub fn typeset(tex: &str, display: bool) -> Result<Arc<Typeset>, String> {
    let tex = tex.trim();
    let style = usize::from(display);
    let kept = MEMO.with(|m| {
        let m = &mut *m.borrow_mut();
        m.now += 1;
        let now = m.now;
        m.styles[style].get_mut(tex).map(|(done, used)| {
            *used = now;
            done.clone()
        })
    });
    if let Some(done) = kept {
        return done;
    }
    let done = build(tex, display).map(Arc::new);
    MEMO.with(|m| {
        let m = &mut *m.borrow_mut();
        if m.styles.iter().map(HashMap::len).sum::<usize>() >= MEMO_CAP {
            let oldest = m
                .styles
                .iter()
                .enumerate()
                .flat_map(|(s, set)| set.iter().map(move |(k, (_, used))| (*used, s, k)))
                .min()
                .map(|(_, s, k)| (s, k.clone()));
            if let Some((s, k)) = oldest {
                m.styles[s].remove(&k);
            }
        }
        let now = m.now;
        m.styles[style].insert(tex.to_owned(), (done.clone(), now));
    });
    done
}

/// [`typeset`] without the memory. A typesetter that panics on a source it was never tried on is an error here naming the
/// source, not a crash of the app (where panics do not unwind, it is the same as any other).
fn build(tex: &str, display: bool) -> Result<Typeset, String> {
    if tex.is_empty() {
        return Err("nothing to typeset".to_owned());
    }
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let nodes = parse(tex).map_err(|e| e.message)?;
        let options = LayoutOptions { style: if display { MathStyle::Display } else { MathStyle::Text }, ..LayoutOptions::default() };
        Ok::<_, String>(to_display_list(&layout(&nodes, &options)))
    }));
    let list = run.map_err(|_| format!("the typesetter panicked on {tex:?}"))??;
    let ink = |c: Color| (c != Color::BLACK).then_some([c.r, c.g, c.b, c.a]);
    let mut items = Vec::with_capacity(list.items.len());
    for item in &list.items {
        items.push(match item {
            DisplayItem::GlyphPath { x, y, scale, font, char_code, color } => {
                // The system's fallback fonts (CJK, emoji) are not bundled: their glyphs come out of the main font, which
                // draws nothing for a character it lacks.
                let id = Katex::parse(font)
                    .filter(|f| !matches!(f, Katex::CjkRegular | Katex::CjkFallback | Katex::EmojiFallback))
                    .unwrap_or(Katex::MainRegular);
                Item::Glyph { font: id, ch: katex_ttf_glyph_char(id, *char_code), at: [*x as f32, *y as f32], em: *scale as f32, ink: ink(*color) }
            }
            DisplayItem::Line { x, y, width, thickness, color, .. } => {
                Item::Rule { rect: [*x as f32, (*y - *thickness * 0.5) as f32, *width as f32, *thickness as f32], ink: ink(*color) }
            }
            DisplayItem::Rect { x, y, width, height, color } => {
                Item::Rule { rect: [*x as f32, *y as f32, *width as f32, *height as f32], ink: ink(*color) }
            }
            DisplayItem::Path { x, y, commands, fill, color } => {
                let segs: Vec<PathSeg> = commands.iter().map(seg).collect();
                let mut h = std::collections::hash_map::DefaultHasher::new();
                format!("{segs:?}").hash(&mut h);
                Item::Path { at: [*x as f32, *y as f32], segs, fill: *fill, ink: ink(*color), key: h.finish() }
            }
        });
    }
    let set = Typeset { width: list.width as f32, height: list.height as f32, depth: list.depth as f32, items };
    if set.items.len() > MAX_ITEMS {
        return Err(format!("{} marks, more than the {MAX_ITEMS} a formula may draw", set.items.len()));
    }
    if !(set.width.is_finite() && set.height.is_finite() && set.depth.is_finite()) || set.width <= 0.0 {
        return Err("nothing to draw".to_owned());
    }
    Ok(set)
}

fn seg(c: &PathCommand) -> PathSeg {
    match *c {
        PathCommand::MoveTo { x, y } => PathSeg::Move([x as f32, y as f32]),
        PathCommand::LineTo { x, y } => PathSeg::Line([x as f32, y as f32]),
        PathCommand::QuadTo { x1, y1, x, y } => PathSeg::Quad([x1 as f32, y1 as f32], [x as f32, y as f32]),
        PathCommand::CubicTo { x1, y1, x2, y2, x, y } => {
            PathSeg::Cubic([x1 as f32, y1 as f32], [x2 as f32, y2 as f32], [x as f32, y as f32])
        }
        PathCommand::Close => PathSeg::Close,
    }
}

/// The file of a KaTeX font (TrueType, under the SIL Open Font License: `fonts/katex/OFL.txt`), bundled so a formula
/// looks the same on every Mac. None for the fonts RaTeX names for system fallbacks.
#[must_use]
pub fn font_file(font: Katex) -> Option<&'static [u8]> {
    Some(match font {
        Katex::AmsRegular => include_bytes!("../fonts/katex/KaTeX_AMS-Regular.ttf"),
        Katex::CaligraphicRegular => include_bytes!("../fonts/katex/KaTeX_Caligraphic-Regular.ttf"),
        Katex::FrakturRegular => include_bytes!("../fonts/katex/KaTeX_Fraktur-Regular.ttf"),
        Katex::FrakturBold => include_bytes!("../fonts/katex/KaTeX_Fraktur-Bold.ttf"),
        Katex::MainBold => include_bytes!("../fonts/katex/KaTeX_Main-Bold.ttf"),
        Katex::MainBoldItalic => include_bytes!("../fonts/katex/KaTeX_Main-BoldItalic.ttf"),
        Katex::MainItalic => include_bytes!("../fonts/katex/KaTeX_Main-Italic.ttf"),
        Katex::MainRegular => include_bytes!("../fonts/katex/KaTeX_Main-Regular.ttf"),
        Katex::MathBoldItalic => include_bytes!("../fonts/katex/KaTeX_Math-BoldItalic.ttf"),
        Katex::MathItalic => include_bytes!("../fonts/katex/KaTeX_Math-Italic.ttf"),
        Katex::SansSerifBold => include_bytes!("../fonts/katex/KaTeX_SansSerif-Bold.ttf"),
        Katex::SansSerifItalic => include_bytes!("../fonts/katex/KaTeX_SansSerif-Italic.ttf"),
        Katex::SansSerifRegular => include_bytes!("../fonts/katex/KaTeX_SansSerif-Regular.ttf"),
        Katex::ScriptRegular => include_bytes!("../fonts/katex/KaTeX_Script-Regular.ttf"),
        Katex::Size1Regular => include_bytes!("../fonts/katex/KaTeX_Size1-Regular.ttf"),
        Katex::Size2Regular => include_bytes!("../fonts/katex/KaTeX_Size2-Regular.ttf"),
        Katex::Size3Regular => include_bytes!("../fonts/katex/KaTeX_Size3-Regular.ttf"),
        Katex::Size4Regular => include_bytes!("../fonts/katex/KaTeX_Size4-Regular.ttf"),
        Katex::TypewriterRegular => include_bytes!("../fonts/katex/KaTeX_Typewriter-Regular.ttf"),
        Katex::CjkRegular | Katex::CjkFallback | Katex::EmojiFallback => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(set: &Typeset) -> Vec<(char, [f32; 2], f32)> {
        set.items
            .iter()
            .filter_map(|i| match i {
                Item::Glyph { ch, at, em, .. } => Some((*ch, *at, *em)),
                _ => None,
            })
            .collect()
    }

    /// A fraction is its numerator over its denominator with a rule between: glyphs and a rule, reaching above and
    /// below the baseline.
    #[test]
    fn a_fraction_typesets_to_glyphs_over_a_rule() {
        let set = typeset("\\frac{a}{b}", true).unwrap();
        let g = glyphs(&set);
        let rules: Vec<[f32; 4]> = set.items.iter().filter_map(|i| if let Item::Rule { rect, .. } = i { Some(*rect) } else { None }).collect();
        assert_eq!(rules.len(), 1, "one bar: {:?}", set.items);
        let at = |c: char| g.iter().find(|x| x.0 == c).unwrap().1;
        assert!(at('a')[1] < rules[0][1] && rules[0][1] < at('b')[1], "a above the bar above b (y runs down): {:?}", set.items);
        assert!(set.width > 0.0 && set.height > 0.0 && set.depth > 0.0);
    }

    /// Display style sets a sum's limits above and below it, text style beside it; a stretchy arrow and tall delimiters
    /// draw outlines (a radical is glyphs from KaTeX's Size fonts and a rule, at any height).
    #[test]
    fn display_style_stacks_limits_and_a_root_draws_an_outline() {
        let src = "\\sum_{i=1}^{n} i";
        let (d, t) = (typeset(src, true).unwrap(), typeset(src, false).unwrap());
        assert!(d.height + d.depth > t.height + t.depth, "stacked limits make it taller: {d:?} against {t:?}");
        for src in ["\\xrightarrow{abc}", "\\left(\\begin{matrix}a\\\\b\\\\c\\\\d\\\\e\\end{matrix}\\right)"] {
            let set = typeset(src, true).unwrap();
            assert!(set.items.iter().any(|i| matches!(i, Item::Path { .. })), "{src}: {set:?}");
        }
    }

    #[test]
    fn a_bad_source_is_an_error_not_a_panic() {
        assert!(typeset("\\frac{1}{", true).is_err());
        assert!(typeset("   ", false).is_err());
    }

    /// The same source is the same formula, remembered (a streaming reply asks for it every frame); the one used longest
    /// ago is forgotten first when the memory is full.
    #[test]
    fn a_formula_is_remembered_and_the_oldest_is_forgotten_first() {
        let (a, b) = (typeset(" x^2 ", false).unwrap(), typeset("x^2", false).unwrap());
        assert!(Arc::ptr_eq(&a, &b), "trimmed, then remembered");
        assert_ne!(*typeset("x^2", true).unwrap(), *a, "display style is another formula");
        let old = typeset("b", false).unwrap();
        for i in 0..MEMO_CAP {
            typeset(&format!("x_{{{i}}}"), false).unwrap();
            assert!(Arc::ptr_eq(&a, &typeset("x^2", false).unwrap()), "the formula in use is kept as {i} more come");
        }
        assert!(!Arc::ptr_eq(&old, &typeset("b", false).unwrap()), "the one not used since went");
    }

    #[test]
    fn every_font_a_formula_can_name_has_its_file() {
        for f in [Katex::MainRegular, Katex::MathItalic, Katex::AmsRegular, Katex::Size4Regular, Katex::TypewriterRegular, Katex::FrakturBold] {
            assert!(font_file(f).is_some_and(|b| b.len() > 1000), "{f:?}");
        }
        assert!(font_file(Katex::EmojiFallback).is_none());
    }
}
