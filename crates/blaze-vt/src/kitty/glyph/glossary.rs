//! The registered glyphs (ghostty's apc/glyph/Glossary.zig): private-use code points to
//! outlines, at most [`MAX_ENTRIES`], the oldest registration evicted first.

use blaze_own::{ABTreeMap, Alloc};

use super::glyf::Outline;
use super::request::{Horizontal, PayloadError, Register, Size, Vertical, Width};

/// The most glyphs held (`max_entries`).
pub const MAX_ENTRIES: usize = 1024;

/// A glyph's design metrics, in font units (font/Glyph.zig `DesignMetrics`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DesignMetrics {
    pub units_per_em: u32,
    pub advance_width: u32,
    pub line_height: u32,
}

/// How the glyph is scaled into its cell (font/Glyph.zig `Constraint.Size`, the values the
/// glyph protocol maps to).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ConstraintSize {
    None,
    Cover,
    Stretch,
}

/// Where the glyph sits on one axis (`Constraint.Align`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ConstraintAlign {
    Start,
    Center,
    End,
}

/// The render constraint of a registered glyph (`RenderOptions.Constraint`, the fields the
/// glyph protocol sets).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Constraint {
    pub size: ConstraintSize,
    pub align_horizontal: ConstraintAlign,
    pub align_vertical: ConstraintAlign,
    pub pad_top: f64,
    pub pad_right: f64,
    pub pad_bottom: f64,
    pub pad_left: f64,
}

/// A registered glyph's outline.
#[derive(Debug)]
pub enum Glyph<'a> {
    Glyf(Outline<'a>),
}

/// Why a register request yields no entry (`Entry.InitError`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryError {
    Payload(PayloadError),
    /// An option is invalid, or a metric is 0.
    InvalidOptions,
    /// `fmt=colrv0` or `fmt=colrv1`.
    UnsupportedFormat,
}

/// One registered glyph.
#[derive(Debug)]
pub struct Entry<'a> {
    pub glyph: Glyph<'a>,
    pub design: DesignMetrics,
    pub width: Width,
    pub constraint: Constraint,
}

impl<'a> Entry<'a> {
    /// The entry a register request describes (`Entry.init`).
    pub fn new(req: &Register<'_>, alloc: Alloc<'a>) -> Result<Entry<'a>, EntryError> {
        let fmt = req.fmt().ok_or(EntryError::InvalidOptions)?;
        let design = DesignMetrics {
            units_per_em: req.upm().ok_or(EntryError::InvalidOptions)?,
            advance_width: req.aw().ok_or(EntryError::InvalidOptions)?,
            line_height: req.lh().ok_or(EntryError::InvalidOptions)?,
        };
        if design.units_per_em == 0 || design.advance_width == 0 || design.line_height == 0 {
            return Err(EntryError::InvalidOptions);
        }
        let width = req.width().ok_or(EntryError::InvalidOptions)?;
        let constraint = constraint_of(req)?;
        let glyph = match fmt {
            super::request::Format::Glyf => {
                Glyph::Glyf(req.decode_glyf_payload(alloc).map_err(EntryError::Payload)?)
            }
            super::request::Format::Colrv0 | super::request::Format::Colrv1 => {
                return Err(EntryError::UnsupportedFormat);
            }
        };
        Ok(Entry { glyph, design, width, constraint })
    }
}

/// `constraintFromRegister`.
fn constraint_of(req: &Register<'_>) -> Result<Constraint, EntryError> {
    let size = req.size().ok_or(EntryError::InvalidOptions)?;
    let align = req.align().ok_or(EntryError::InvalidOptions)?;
    let pad = req.pad().ok_or(EntryError::InvalidOptions)?;
    Ok(Constraint {
        size: match size {
            Size::Height | Size::Advance => ConstraintSize::None,
            Size::Contain | Size::Cover => ConstraintSize::Cover,
            Size::Stretch => ConstraintSize::Stretch,
        },
        align_horizontal: match align.horizontal {
            Horizontal::Start => ConstraintAlign::Start,
            Horizontal::Center => ConstraintAlign::Center,
            Horizontal::End => ConstraintAlign::End,
        },
        align_vertical: match align.vertical {
            Vertical::Start | Vertical::Baseline => ConstraintAlign::Start,
            Vertical::Center => ConstraintAlign::Center,
            Vertical::End => ConstraintAlign::End,
        },
        pad_top: pad.top,
        pad_right: pad.right,
        pad_bottom: pad.bottom,
        pad_left: pad.left,
    })
}

/// Why the glossary refused a code point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlossaryError {
    /// Not a private-use code point.
    OutOfNamespace,
    OutOfMemory,
}

/// The registered glyphs, oldest first.
#[derive(Debug)]
pub struct Glossary<'a> {
    /// Code point to (registration stamp, entry).
    entries: ABTreeMap<'a, u32, (u64, Entry<'a>)>,
    /// Registration stamp to code point: the eviction order.
    order: ABTreeMap<'a, u64, u32>,
    next_stamp: u64,
}

impl<'a> Glossary<'a> {
    pub fn new(alloc: Alloc<'a>) -> Glossary<'a> {
        Glossary {
            entries: ABTreeMap::new_in(alloc),
            order: ABTreeMap::new_in(alloc),
            next_stamp: 0,
        }
    }

    /// Register `entry` at `cp` (`register`): a code point already held is replaced and
    /// becomes the newest; past [`MAX_ENTRIES`] the oldest goes.
    pub fn register(&mut self, cp: u32, entry: Entry<'a>) -> Result<(), GlossaryError> {
        if !is_private_use(cp) {
            return Err(GlossaryError::OutOfNamespace);
        }
        let stamp = self.next_stamp;
        self.order.try_insert(stamp, cp).map_err(|_| GlossaryError::OutOfMemory)?;
        match self.entries.try_insert(cp, (stamp, entry)) {
            Ok(None) => {}
            Ok(Some((old, _))) => {
                self.order.remove(&old);
            }
            Err(_) => {
                self.order.remove(&stamp);
                return Err(GlossaryError::OutOfMemory);
            }
        }
        self.next_stamp += 1;
        if self.entries.len() > MAX_ENTRIES {
            if let Some((_, oldest)) = self.order.pop_first() {
                self.entries.remove(&oldest);
            }
        }
        Ok(())
    }

    /// Remove the glyph at `cp` (`delete`); an empty slot is fine.
    pub fn delete(&mut self, cp: u32) -> Result<(), GlossaryError> {
        if !is_private_use(cp) {
            return Err(GlossaryError::OutOfNamespace);
        }
        if let Some((stamp, _)) = self.entries.remove(&cp) {
            self.order.remove(&stamp);
        }
        Ok(())
    }

    /// Remove every glyph (`clearAndFree`).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    pub fn contains(&self, cp: u32) -> bool {
        self.entries.get(&cp).is_some()
    }

    pub fn get(&self, cp: u32) -> Option<&Entry<'a>> {
        self.entries.get(&cp).map(|(_, e)| e)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }

    /// The code points held, oldest registration first.
    pub fn codepoints(&self) -> impl Iterator<Item = u32> + '_ {
        self.order.values().copied()
    }
}

/// The three private-use areas.
pub fn is_private_use(cp: u32) -> bool {
    (0xE000..=0xF8FF).contains(&cp)
        || (0xF0000..=0xFFFFD).contains(&cp)
        || (0x100000..=0x10FFFD).contains(&cp)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::kitty::glyph::request::{Parser, Request};
    use crate::kitty::storage::tests::alloc;
    use std::format;

    pub(crate) const TRIANGLE: &str = "AAEAZABkA4QDhAACAAABAQEB9P5wAyADhPzgAAA=";

    pub(crate) fn register_req(data: &[u8]) -> Register<'static> {
        let mut p = Parser::new(1024 * 1024, alloc());
        p.feed_slice(data).unwrap();
        match p.complete().unwrap() {
            Request::Register(r) => r,
            other => panic!("not a register: {other:?}"),
        }
    }

    fn triangle_req(cp: u32) -> Register<'static> {
        let s = format!(
            "r;cp={cp:x};upm=2048;aw=1024;lh=1536;width=2;size=stretch;align=end,start;pad=0.1,0.2,0.3,0.4;{TRIANGLE}"
        );
        register_req(s.as_bytes())
    }

    fn entry(cp: u32) -> Entry<'static> {
        Entry::new(&triangle_req(cp), alloc()).unwrap()
    }

    fn order(g: &Glossary<'_>) -> std::vec::Vec<u32> {
        g.codepoints().collect()
    }

    // ghostty Glossary.zig:297 "Entry init decodes glyf payload and applies register fields"
    #[test]
    fn entry_init_decodes_glyf_payload_and_applies_register_fields() {
        let e = entry(0xE000);
        assert_eq!(
            e.design,
            DesignMetrics { units_per_em: 2048, advance_width: 1024, line_height: 1536 }
        );
        assert_eq!(e.width, Width::Wide);
        assert_eq!(e.constraint.size, ConstraintSize::Stretch);
        assert_eq!(e.constraint.align_horizontal, ConstraintAlign::End);
        assert_eq!(e.constraint.align_vertical, ConstraintAlign::Start);
        let c = e.constraint;
        assert_eq!((c.pad_top, c.pad_right, c.pad_bottom, c.pad_left), (0.1, 0.2, 0.3, 0.4));
        let Glyph::Glyf(outline) = &e.glyph;
        assert_eq!((outline.points.len(), outline.contours.len()), (3, 1));
    }

    // ghostty Glossary.zig:323 "Entry init rejects invalid register payload"
    #[test]
    fn entry_init_rejects_invalid_register_payload() {
        let req = register_req(b"r;cp=e000;%%%not-base64%%%");
        assert_eq!(
            Entry::new(&req, alloc()).unwrap_err(),
            EntryError::Payload(PayloadError::MalformedPayload)
        );
        let bad = |data: &[u8]| Entry::new(&register_req(data), alloc()).unwrap_err();
        assert_eq!(bad(b"r;cp=e000;upm=0;AAAAAAAAAAAAAA=="), EntryError::InvalidOptions);
        assert_eq!(bad(b"r;cp=e000;aw=0;AAAAAAAAAAAAAA=="), EntryError::InvalidOptions);
        assert_eq!(bad(b"r;cp=e000;width=3;AAAAAAAAAAAAAA=="), EntryError::InvalidOptions);
        assert_eq!(bad(b"r;cp=e000;size=x;AAAAAAAAAAAAAA=="), EntryError::InvalidOptions);
        assert_eq!(bad(b"r;cp=e000;fmt=x;AAAAAAAAAAAAAA=="), EntryError::InvalidOptions);
        assert_eq!(bad(b"r;cp=e000;fmt=colrv1;AAAAAAAAAAAAAA=="), EntryError::UnsupportedFormat);
        // Constraint mapping: contain reads as cover, baseline as start.
        let e = Entry::new(
            &register_req(b"r;size=contain;align=start,baseline;AAAAAAAAAAAAAA=="),
            alloc(),
        )
        .unwrap();
        assert_eq!(e.constraint.size, ConstraintSize::Cover);
        assert_eq!(
            (e.constraint.align_horizontal, e.constraint.align_vertical),
            (ConstraintAlign::Start, ConstraintAlign::Start)
        );
    }

    // ghostty Glossary.zig:333 "Glossary register overwrites and moves entry to newest position"
    #[test]
    fn glossary_register_overwrites_and_moves_entry_to_newest_position() {
        let mut g = Glossary::new(alloc());
        g.register(0xE000, entry(0xE000)).unwrap();
        g.register(0xE001, entry(0xE001)).unwrap();
        g.register(0xE000, entry(0xE000)).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(order(&g), [0xE001, 0xE000]);
    }

    // ghostty Glossary.zig:349 "Glossary register evicts oldest entry"
    #[test]
    fn glossary_register_evicts_oldest_entry() {
        let mut g = Glossary::new(alloc());
        for i in 0..=MAX_ENTRIES as u32 {
            g.register(0xE000 + i, entry(0xE000 + i)).unwrap();
        }
        assert_eq!(g.len(), MAX_ENTRIES);
        assert!(!g.contains(0xE000));
        assert!(g.contains(0xE001));
        assert!(g.contains(0xE000 + MAX_ENTRIES as u32));
    }

    // ghostty Glossary.zig:367 "Glossary register rejects non-PUA codepoint"
    #[test]
    fn glossary_register_rejects_non_pua_codepoint() {
        let mut g = Glossary::new(alloc());
        assert_eq!(g.register(u32::from('A'), entry(0xE000)), Err(GlossaryError::OutOfNamespace));
        for cp in [0xDFFF, 0xF900, 0xEFFFF, 0xFFFFE, 0x10FFFE] {
            assert!(!is_private_use(cp), "{cp:x}");
        }
        for cp in [0xE000, 0xF8FF, 0xF0000, 0xFFFFD, 0x100000, 0x10FFFD] {
            assert!(is_private_use(cp), "{cp:x}");
        }
    }

    // ghostty Glossary.zig:379 "Glossary delete removes one PUA slot and ignores empty PUA slot"
    #[test]
    fn glossary_delete_removes_one_pua_slot_and_ignores_empty_pua_slot() {
        let mut g = Glossary::new(alloc());
        g.register(0xE000, entry(0xE000)).unwrap();
        g.register(0xE001, entry(0xE001)).unwrap();
        g.delete(0xE000).unwrap();
        assert_eq!(g.len(), 1);
        assert!(!g.contains(0xE000) && g.contains(0xE001));
        g.delete(0xE000).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(order(&g), [0xE001]);
    }

    // ghostty Glossary.zig:399 "Glossary delete rejects non-PUA codepoint"
    #[test]
    fn glossary_delete_rejects_non_pua_codepoint() {
        let mut g = Glossary::new(alloc());
        g.register(0xE000, entry(0xE000)).unwrap();
        assert_eq!(g.delete(u32::from('A')), Err(GlossaryError::OutOfNamespace));
        assert_eq!(g.len(), 1);
        assert!(g.contains(0xE000));
    }

    // ghostty Glossary.zig:412 "Glossary clearAndFree removes all slots and remains reusable"
    #[test]
    fn glossary_clear_and_free_removes_all_slots_and_remains_reusable() {
        let mut g = Glossary::new(alloc());
        g.register(0xE000, entry(0xE000)).unwrap();
        g.register(0xE001, entry(0xE001)).unwrap();
        g.clear();
        assert!(g.is_empty());
        assert!(!g.contains(0xE000) && !g.contains(0xE001));
        g.register(0xE002, entry(0xE002)).unwrap();
        assert_eq!(g.len(), 1);
        assert!(g.contains(0xE002));
        assert_eq!(order(&g), [0xE002]);
    }

    // ghostty Glossary.zig:432 "Glossary contains reports registered slots"
    #[test]
    fn glossary_contains_reports_registered_slots() {
        let mut g = Glossary::new(alloc());
        assert!(!g.contains(0xE000));
        g.register(0xE000, entry(0xE000)).unwrap();
        assert!(g.contains(0xE000));
        assert!(!g.contains(0xE001));
        assert!(g.get(0xE000).is_some());
    }
}
