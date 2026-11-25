//! A screen: its rows ([`PageList`]), what their cells name out of line (interned styles,
//! grapheme codepoints, hyperlinks), the cursor, the saved cursor, the charsets, protection,
//! and the kitty keyboard flags. Ports ghostty's Screen.zig and the parts of PageList.zig a
//! terminal reaches (scroll, erase history, scroll clear, resize with and without reflow).
//!
//! Every out-of-line thing is reference-counted by the cells that name it (and the cursor, for
//! its style and hyperlink): a cell overwritten or cleared releases what it named, so nothing
//! outlives its last cell. The tables are per screen, so a row moved anywhere in its screen
//! (scroll, reflow) keeps its ids as they are.

use core::hash::{Hash, Hasher};

use blaze_own::planted::Planted;
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::input::key::KittyFlags;
use crate::page::{Cell, MAX_EXT, PageList, Row, RowPrompt, SemanticContent, Wide};
use crate::plant::VtPlant;
use crate::style::{Color, Style};

/// Why the screen could not keep something, counted in [`ScreenStats`] (the screen degrades as
/// ghostty does: a style falls back to the default, a grapheme codepoint or hyperlink is
/// dropped).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The allocator refused.
    Alloc(AllocFailure),
    /// 65535 distinct styles are live: a cell's style id is 16 bits.
    Styles,
    /// [`MAX_EXT`] cells hold graphemes or hyperlinks.
    Ext,
}

impl From<AllocFailure> for Refused {
    fn from(f: AllocFailure) -> Refused {
        Refused::Alloc(f)
    }
}

/// What a screen has refused since it was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScreenStats {
    pub styles_refused: u64,
    pub ext_refused: u64,
    pub alloc_failed: u64,
    /// The first allocation failure, kept for the caller.
    pub first_failure: Option<AllocFailure>,
}

impl ScreenStats {
    pub fn note(&mut self, why: Refused) {
        match why {
            Refused::Alloc(f) => {
                self.alloc_failed += 1;
                if self.first_failure.is_none() {
                    self.first_failure = Some(f);
                }
            }
            Refused::Styles => self.styles_refused += 1,
            Refused::Ext => self.ext_refused += 1,
        }
    }
}

/// FNV-1a over what `Hash` writes: the styles table's hash (no seed is needed: a style's bytes
/// come from SGR parameters, at most 65535 live).
struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
}

fn style_hash(s: &Style) -> u64 {
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    s.hash(&mut h);
    h.finish()
}

#[derive(Clone, Copy)]
struct StyleEntry {
    style: Style,
    refs: u32,
}

/// Interned styles: id 0 is the default style and is never counted; every other id is held by
/// the cells (and cursors) that name it, and freed when the last lets go.
pub struct StyleSet<'a> {
    entries: Vec<StyleEntry, Alloc<'a>>,
    free: Vec<u16, Alloc<'a>>,
    /// Open addressing, linear probing: 0 is an empty slot, else an id.
    table: Vec<u16, Alloc<'a>>,
    live: usize,
}

impl<'a> StyleSet<'a> {
    pub fn new(alloc: Alloc<'a>) -> StyleSet<'a> {
        let mut entries = Vec::new_in(alloc);
        entries.push(StyleEntry { style: Style::default(), refs: 0 });
        StyleSet { entries, free: Vec::new_in(alloc), table: Vec::new_in(alloc), live: 0 }
    }

    /// Distinct non-default styles live.
    pub fn live(&self) -> usize {
        self.live
    }

    pub fn get(&self, id: u16) -> &Style {
        &self.entries[usize::from(id)].style
    }

    /// References held on `id`.
    pub fn refs(&self, id: u16) -> u32 {
        self.entries[usize::from(id)].refs
    }

    fn mask(&self) -> usize {
        self.table.len() - 1
    }

    fn find(&self, style: &Style) -> Option<u16> {
        if self.table.is_empty() {
            return None;
        }
        let mut i = style_hash(style) as usize & self.mask();
        loop {
            let id = self.table[i];
            if id == 0 {
                return None;
            }
            if self.entries[usize::from(id)].style == *style {
                return Some(id);
            }
            i = (i + 1) & self.mask();
        }
    }

    fn place(&mut self, id: u16) {
        let mut i = style_hash(&self.entries[usize::from(id)].style) as usize & self.mask();
        while self.table[i] != 0 {
            i = (i + 1) & self.mask();
        }
        self.table[i] = id;
    }

    fn rehash(&mut self, size: usize) -> Result<(), AllocFailure> {
        let mut table = Vec::try_with_capacity_in(size, *self.table_alloc())?;
        table.resize(size, 0u16);
        self.table = table;
        for id in 1..self.entries.len() {
            if self.entries[id].refs > 0 {
                self.place(id as u16);
            }
        }
        Ok(())
    }

    fn table_alloc(&self) -> &Alloc<'a> {
        self.entries.allocator()
    }

    /// The id of `style` with `n` more references on it, interned if new.
    pub fn acquire(&mut self, style: &Style, n: u32) -> Result<u16, Refused> {
        if style.is_default() || n == 0 {
            return Ok(0);
        }
        if let Some(id) = self.find(style) {
            self.entries[usize::from(id)].refs += n;
            return Ok(id);
        }
        if (self.live + 1) * 2 > self.table.len() {
            self.rehash((self.table.len() * 2).max(16))?;
        }
        let id = match self.free.pop() {
            Some(id) => {
                self.entries[usize::from(id)] = StyleEntry { style: *style, refs: n };
                id
            }
            None => {
                if self.entries.len() > usize::from(u16::MAX) {
                    return Err(Refused::Styles);
                }
                if let Err(refused) = self.entries.try_push(StyleEntry { style: *style, refs: n }) {
                    drop(refused);
                    return Err(Refused::Alloc(AllocFailure::Refused {
                        what: "blaze_vt::screen::StyleSet::entries",
                        layout: core::alloc::Layout::new::<StyleEntry>(),
                    }));
                }
                (self.entries.len() - 1) as u16
            }
        };
        self.live += 1;
        self.place(id);
        Ok(id)
    }

    /// `n` more references on `id`.
    pub fn add(&mut self, id: u16, n: u32) {
        if id != 0 {
            self.entries[usize::from(id)].refs += n;
        }
    }

    /// `n` references let go of `id`; the last frees it.
    pub fn release(&mut self, id: u16, n: u32) {
        if id == 0 || n == 0 {
            return;
        }
        let e = &mut self.entries[usize::from(id)];
        assert!(e.refs >= n, "style {id} released {n} times with {} references", e.refs);
        e.refs -= n;
        if e.refs > 0 {
            return;
        }
        // Remove from the table by backward shift.
        let mask = self.mask();
        let mut i = style_hash(&self.entries[usize::from(id)].style) as usize & mask;
        while self.table[i] != id {
            i = (i + 1) & mask;
        }
        self.table[i] = 0;
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let other = self.table[j];
            if other == 0 {
                break;
            }
            let k = style_hash(&self.entries[usize::from(other)].style) as usize & mask;
            let movable = if i <= j { k <= i || k > j } else { k <= i && k > j };
            if movable {
                self.table[i] = other;
                self.table[j] = 0;
                i = j;
            }
        }
        self.live -= 1;
        self.free.push(id);
    }
}

#[derive(Clone, Copy, Default)]
struct Ext {
    /// The hyperlink id, 0 for none.
    link: u32,
    /// The grapheme codepoints after the cell's first, in `cps`.
    start: u32,
    len: u16,
    cap: u16,
}

/// Cells' out-of-line entries: the codepoints of a grapheme after its first, and a hyperlink
/// id. Each entry is owned by exactly one cell.
pub struct ExtTable<'a> {
    entries: Vec<Ext, Alloc<'a>>,
    free: Vec<u32, Alloc<'a>>,
    cps: Vec<u32, Alloc<'a>>,
    /// Slots of `cps` no entry uses any more.
    dead: usize,
    live: usize,
}

impl<'a> ExtTable<'a> {
    pub fn new(alloc: Alloc<'a>) -> ExtTable<'a> {
        let mut entries = Vec::new_in(alloc);
        entries.push(Ext::default());
        ExtTable { entries, free: Vec::new_in(alloc), cps: Vec::new_in(alloc), dead: 0, live: 0 }
    }

    pub fn live(&self) -> usize {
        self.live
    }

    fn take(&mut self) -> Result<u32, Refused> {
        let id = match self.free.pop() {
            Some(id) => id,
            None => {
                if self.entries.len() > MAX_EXT as usize {
                    return Err(Refused::Ext);
                }
                self.entries.try_reserve(1)?;
                self.entries.push(Ext::default());
                (self.entries.len() - 1) as u32
            }
        };
        self.entries[id as usize] = Ext::default();
        self.live += 1;
        Ok(id)
    }

    fn give(&mut self, id: u32) {
        let e = self.entries[id as usize];
        self.dead += usize::from(e.cap);
        self.entries[id as usize] = Ext::default();
        self.free.push(id);
        self.live -= 1;
    }

    pub fn graphemes(&self, id: u32) -> &[u32] {
        let e = self.entries[id as usize];
        &self.cps[e.start as usize..e.start as usize + usize::from(e.len)]
    }

    pub fn link(&self, id: u32) -> u32 {
        self.entries[id as usize].link
    }

    fn push_cp(&mut self, id: u32, cp: u32) -> Result<(), Refused> {
        let e = self.entries[id as usize];
        if e.len == u16::MAX {
            return Err(Refused::Ext);
        }
        if e.len == e.cap {
            let cap = (e.cap.max(1)).saturating_mul(2);
            let start = self.cps.len();
            self.cps.try_reserve(usize::from(cap))?;
            for i in 0..usize::from(e.len) {
                let v = self.cps[e.start as usize + i];
                self.cps.push(v);
            }
            self.cps.resize(start + usize::from(cap), 0);
            self.dead += usize::from(e.cap);
            self.entries[id as usize].start = start as u32;
            self.entries[id as usize].cap = cap;
        }
        let e = &mut self.entries[id as usize];
        self.cps[e.start as usize + usize::from(e.len)] = cp;
        e.len += 1;
        Ok(())
    }

    fn clear_graphemes(&mut self, id: u32) {
        let e = &mut self.entries[id as usize];
        self.dead += usize::from(e.cap);
        e.start = 0;
        e.len = 0;
        e.cap = 0;
    }

    /// Packs `cps` when most of it is dead.
    fn compact(&mut self) -> Result<(), AllocFailure> {
        if self.dead < 4096 || self.dead * 2 < self.cps.len() {
            return Ok(());
        }
        let mut cps = Vec::try_with_capacity_in(self.cps.len() - self.dead, *self.cps.allocator())?;
        for e in self.entries.iter_mut() {
            if e.cap == 0 {
                continue;
            }
            let start = cps.len();
            for i in 0..usize::from(e.len) {
                cps.push(self.cps[e.start as usize + i]);
            }
            e.start = start as u32;
            e.cap = e.len;
        }
        self.cps = cps;
        self.dead = 0;
        Ok(())
    }
}

/// A hyperlink's id: the one OSC 8 gave, or one made for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkId<'s> {
    Explicit(&'s [u8]),
    Implicit(u32),
}

struct Link<'a> {
    uri: Vec<u8, Alloc<'a>>,
    explicit: Option<Vec<u8, Alloc<'a>>>,
    implicit: u32,
    refs: u32,
}

/// Hyperlinks, held by the cells in them and the cursor while one is open.
pub struct LinkTable<'a> {
    alloc: Alloc<'a>,
    links: Vec<Option<Link<'a>>, Alloc<'a>>,
    free: Vec<u32, Alloc<'a>>,
    next_implicit: u32,
}

impl<'a> LinkTable<'a> {
    pub fn new(alloc: Alloc<'a>) -> LinkTable<'a> {
        let mut links = Vec::new_in(alloc);
        links.push(None);
        LinkTable { alloc, links, free: Vec::new_in(alloc), next_implicit: 0 }
    }

    /// Live hyperlinks.
    pub fn live(&self) -> usize {
        self.links.iter().filter(|l| l.is_some()).count()
    }

    /// The link `id`'s URI and id.
    pub fn get(&self, id: u32) -> Option<(&[u8], LinkId<'_>)> {
        let l = self.links.get(id as usize)?.as_ref()?;
        let lid = match &l.explicit {
            Some(e) => LinkId::Explicit(e.as_slice()),
            None => LinkId::Implicit(l.implicit),
        };
        Some((l.uri.as_slice(), lid))
    }

    /// A link to `uri` with `id` (or a fresh implicit one), one reference held; an open link
    /// with the same explicit id and URI is shared.
    pub fn open(&mut self, uri: &[u8], id: Option<&[u8]>) -> Result<u32, Refused> {
        if let Some(want) = id {
            for (i, l) in self.links.iter_mut().enumerate() {
                if let Some(l) = l
                    && l.uri.as_slice() == uri
                    && l.explicit.as_ref().is_some_and(|e| e.as_slice() == want)
                {
                    l.refs += 1;
                    return Ok(i as u32);
                }
            }
        }
        let mut u = Vec::try_with_capacity_in(uri.len(), self.alloc)?;
        u.extend_from_slice(uri);
        let explicit = match id {
            Some(v) => {
                let mut e = Vec::try_with_capacity_in(v.len(), self.alloc)?;
                e.extend_from_slice(v);
                Some(e)
            }
            None => None,
        };
        self.next_implicit += 1;
        let link = Link { uri: u, explicit, implicit: self.next_implicit, refs: 1 };
        match self.free.pop() {
            Some(i) => {
                self.links[i as usize] = Some(link);
                Ok(i)
            }
            None => {
                self.links.try_reserve(1)?;
                self.links.push(Some(link));
                Ok((self.links.len() - 1) as u32)
            }
        }
    }

    pub fn add(&mut self, id: u32) {
        if let Some(Some(l)) = self.links.get_mut(id as usize) {
            l.refs += 1;
        }
    }

    pub fn release(&mut self, id: u32) {
        if id == 0 {
            return;
        }
        let slot = &mut self.links[id as usize];
        let l = slot.as_mut().expect("a hyperlink released after it was freed");
        l.refs -= 1;
        if l.refs == 0 {
            *slot = None;
            self.free.push(id);
        }
    }
}

/// The four charset tables a slot can name (charsets.zig).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Charset {
    #[default]
    Utf8,
    Ascii,
    British,
    DecSpecial,
}

/// G0–G3.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Slot {
    #[default]
    G0 = 0,
    G1 = 1,
    G2 = 2,
    G3 = 3,
}

/// The designated charsets and what GL, GR and a single shift invoke (Screen.zig:199).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Charsets {
    pub sets: [Charset; 4],
    pub gl: Slot,
    pub gr: Slot,
    pub single_shift: Option<Slot>,
}

impl Default for Charsets {
    fn default() -> Charsets {
        Charsets { sets: [Charset::Utf8; 4], gl: Slot::G0, gr: Slot::G2, single_shift: None }
    }
}

/// DEC Special Graphics for 0x60–0x7E (charsets.zig `dec_special`).
const DEC_SPECIAL: [u32; 31] = [
    0x25C6, 0x2592, 0x2409, 0x240C, 0x240D, 0x240A, 0x00B0, 0x00B1, 0x2424, 0x240B, 0x2518, 0x2510,
    0x250C, 0x2514, 0x253C, 0x23BA, 0x23BB, 0x2500, 0x23BC, 0x23BD, 0x251C, 0x2524, 0x2534, 0x252C,
    0x2502, 0x2264, 0x2265, 0x03C0, 0x2260, 0x00A3, 0x00B7,
];

/// `cp` through charset `set` (Terminal.zig `printCell`, 1242): UTF-8 and ASCII as is; a
/// table charset maps a byte, and anything past 0xFF becomes a space.
pub fn map_charset(set: Charset, cp: u32) -> u32 {
    match set {
        Charset::Utf8 | Charset::Ascii => cp,
        _ if cp > 0xFF => u32::from(b' '),
        Charset::British if cp == 0x23 => 0xA3,
        Charset::DecSpecial if (0x60..=0x7E).contains(&cp) => DEC_SPECIAL[(cp - 0x60) as usize],
        _ => cp,
    }
}

/// The shape DECSCUSR asks the cursor to take.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
    BlockHollow,
}

/// The cursor (Screen.zig `Cursor`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    pub x: usize,
    pub y: usize,
    pub pending_wrap: bool,
    pub protected: bool,
    pub style: Style,
    /// `style`'s id in the screen's set, one reference held by the cursor.
    pub style_id: u16,
    pub shape: CursorShape,
    /// The open hyperlink's id in the screen's table, 0 for none; one reference held.
    pub hyperlink: u32,
    pub semantic: SemanticContent,
    pub semantic_clear_eol: bool,
}

impl Cursor {
    const HOME: Cursor = Cursor {
        x: 0,
        y: 0,
        pending_wrap: false,
        protected: false,
        style: Style {
            fg: Color::None,
            bg: Color::None,
            underline_color: Color::None,
            flags: crate::style::Flags {
                bold: false,
                italic: false,
                faint: false,
                blink: false,
                inverse: false,
                invisible: false,
                strikethrough: false,
                overline: false,
                underline: crate::style::Underline::None,
            },
        },
        style_id: 0,
        shape: CursorShape::Block,
        hyperlink: 0,
        semantic: SemanticContent::Output,
        semantic_clear_eol: false,
    };
}

/// What DECSC saves (Screen.zig `SavedCursor`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SavedCursor {
    pub x: usize,
    pub y: usize,
    pub style: Style,
    pub protected: bool,
    pub pending_wrap: bool,
    pub origin: bool,
    pub charset: Charsets,
}

/// The last protection mode asked for: ISO (SPA) makes every erase respect protection, even
/// after EPA; DEC (DECSCA) only the selective erases (Terminal.zig:1709).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ProtectedMode {
    #[default]
    Off,
    Iso,
    Dec,
}

/// The kitty keyboard flags stack, 8 entries, the oldest evicted when full (kitty/key.zig
/// `KeyFlagStack`). The flags themselves are the key encoder's [`KittyFlags`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct KittyFlagStack {
    flags: [KittyFlags; 8],
    idx: u8,
}

/// How `CSI = flags ; mode u` combines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KittySet {
    Set,
    Or,
    Not,
}

impl KittyFlagStack {
    pub fn current(&self) -> KittyFlags {
        self.flags[usize::from(self.idx)]
    }
    /// `CSI = v ; mode u`, `v` the protocol's integer.
    pub fn set(&mut self, mode: KittySet, v: u8) {
        let f = &mut self.flags[usize::from(self.idx)];
        let cur = f.bits();
        *f = KittyFlags::from_bits(match mode {
            KittySet::Set => v,
            KittySet::Or => cur | v,
            KittySet::Not => cur & !v,
        });
    }
    /// `CSI > v u`.
    pub fn push(&mut self, v: u8) {
        self.idx = (self.idx + 1) & 7;
        self.flags[usize::from(self.idx)] = KittyFlags::from_bits(v);
    }
    pub fn pop(&mut self, n: usize) {
        if n >= 8 {
            *self = KittyFlagStack::default();
            return;
        }
        for _ in 0..n {
            self.flags[usize::from(self.idx)] = KittyFlags::DISABLED;
            self.idx = self.idx.wrapping_sub(1) & 7;
        }
    }
}

/// What a resize asks of a screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resize {
    pub cols: usize,
    pub rows: usize,
    /// Soft-wrapped rows are rewrapped to the new width (the primary screen with DECAWM).
    pub reflow: bool,
    /// Clear the prompt the cursor is in so the shell redraws it (OSC 133, kitty `redraw`).
    pub prompt_redraw: PromptRedraw,
}

/// What [`Screen::set_semantic_content`] sets (ghostty's `cursorSetSemanticContent` union).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SemanticSet {
    Output,
    /// `clear_eol`: the next linefeed puts the cursor back to output (OSC 133 B's default).
    Input {
        clear_eol: bool,
    },
    /// A prompt; `continuation` marks the row a continuation of the prompt above.
    Prompt {
        continuation: bool,
    },
}

/// OSC 133's `redraw` option (ghostty's `osc.semantic_prompt.Redraw`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PromptRedraw {
    False,
    #[default]
    True,
    Last,
}

/// A screen: primary or alternate.
pub struct Screen<'a, P> {
    alloc: Alloc<'a>,
    pub pages: PageList<'a, P>,
    pub styles: StyleSet<'a>,
    pub ext: ExtTable<'a>,
    pub links: LinkTable<'a>,
    pub cursor: Cursor,
    pub saved: Option<SavedCursor>,
    pub charset: Charsets,
    pub protected_mode: ProtectedMode,
    pub kitty: KittyFlagStack,
    /// The alternate screen keeps no scrollback.
    pub no_scrollback: bool,
    /// Every row needs drawing (ghostty's `dirty.clear`).
    pub all_dirty: bool,
    pub stats: ScreenStats,
    /// OSC 133 state (ghostty's `Screen.semantic_prompt`).
    pub semantic_prompt: SemanticPromptState,
    /// Pins others hold (kitty placements), moved with their cells.
    pins: Vec<TrackedPin, Alloc<'a>>,
    next_pin: u64,
    plant: P,
}

/// How clicks in a prompt are handled, from the last OSC 133 `A` that said
/// (ghostty's `SemanticPrompt.SemanticClick`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PromptClick {
    #[default]
    None,
    ClickEvents(crate::osc::ClickEvents),
    Cl(crate::osc::Click),
}

/// OSC 133 state of a screen (Screen.zig:96 `SemanticPrompt`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SemanticPromptState {
    /// A prompt was ever marked on this screen.
    pub seen: bool,
    pub click: PromptClick,
}

/// Where a pin sits through a resize: a row and column of the page list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pin {
    r: usize,
    x: usize,
    /// Moved into the new rows already (a reflow moves each pin once).
    moved: bool,
}

/// A pin [`Screen::track_pin`] gave out: a page-list row and column. Rows leaving the front
/// move it up; a pin whose row is gone is [`LOST`] (ghostty moves such a pin to the top left).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TrackedPin {
    id: u64,
    r: usize,
    x: usize,
}

/// A tracked pin's row once its row is gone.
const LOST: usize = usize::MAX - 1;

impl<'a, P: Planted<VtPlant>> Screen<'a, P> {
    /// A screen of `rows` × `cols` blank cells.
    pub fn new(
        cols: usize,
        rows: usize,
        page_rows: usize,
        max_scrollback_bytes: usize,
        no_scrollback: bool,
        alloc: Alloc<'a>,
        plant: P,
    ) -> Result<Screen<'a, P>, AllocFailure> {
        Ok(Screen {
            alloc,
            pages: PageList::new(cols, rows, page_rows, max_scrollback_bytes, alloc, plant)?,
            styles: StyleSet::new(alloc),
            ext: ExtTable::new(alloc),
            links: LinkTable::new(alloc),
            cursor: Cursor::HOME,
            saved: None,
            charset: Charsets::default(),
            protected_mode: ProtectedMode::Off,
            kitty: KittyFlagStack::default(),
            no_scrollback,
            all_dirty: true,
            stats: ScreenStats::default(),
            semantic_prompt: SemanticPromptState::default(),
            pins: Vec::new_in(alloc),
            next_pin: 0,
            plant,
        })
    }

    // ---- tracked pins ----------------------------------------------------------------------

    /// Tracks the cell at page-list row `r`, column `x` (ghostty's `trackPin`): the id names it
    /// until [`untrack_pin`](Self::untrack_pin).
    pub fn track_pin(&mut self, r: usize, x: usize) -> Result<u64, AllocFailure> {
        self.pins.try_reserve(1)?;
        let id = self.next_pin;
        self.next_pin += 1;
        self.pins.push(TrackedPin { id, r, x });
        Ok(id)
    }

    pub fn untrack_pin(&mut self, id: u64) {
        if let Some(i) = self.pins.iter().position(|p| p.id == id) {
            self.pins.remove_swap(i);
        }
    }

    /// Where a tracked pin is: its page-list row and column, the top left once its row is gone.
    pub fn pin(&self, id: u64) -> Option<(usize, usize)> {
        let p = self.pins.iter().find(|p| p.id == id)?;
        Some(if p.r == LOST { (0, 0) } else { (p.r, p.x) })
    }

    /// Pins tracked.
    pub fn pins_tracked(&self) -> usize {
        self.pins.len()
    }

    /// `n` rows left the front of the page list.
    fn pins_shift(&mut self, n: usize) {
        for p in self.pins.iter_mut() {
            if p.r != LOST {
                p.r = p.r.checked_sub(n).unwrap_or(LOST);
            }
        }
    }

    pub fn cols(&self) -> usize {
        self.pages.cols()
    }

    pub fn rows(&self) -> usize {
        self.pages.rows()
    }

    /// The page-list row of active row `y`.
    pub fn r(&self, y: usize) -> usize {
        self.pages.active_top() + y
    }

    /// The page-list row the cursor is on.
    pub fn cursor_r(&self) -> usize {
        self.r(self.cursor.y)
    }

    pub fn cursor_cell(&self) -> Cell {
        self.pages.cell(self.cursor_r(), self.cursor.x)
    }

    pub fn cursor_row(&self) -> &Row {
        self.pages.row(self.cursor_r())
    }

    pub fn cursor_row_mut(&mut self) -> &mut Row {
        let r = self.cursor_r();
        self.pages.row_mut(r)
    }

    pub fn mark_cursor_dirty(&mut self) {
        self.cursor_row_mut().dirty = true;
    }

    /// The style `id` names in this screen.
    pub fn style(&self, id: u16) -> &Style {
        self.styles.get(id)
    }

    /// The codepoints of a cell: its first, then its grapheme's others.
    pub fn graphemes(&self, cell: Cell) -> &[u32] {
        if cell.has_grapheme() { self.ext.graphemes(cell.ext()) } else { &[] }
    }

    /// The hyperlink a cell is in, 0 for none.
    pub fn cell_link(&self, cell: Cell) -> u32 {
        if cell.has_hyperlink() { self.ext.link(cell.ext()) } else { 0 }
    }

    pub(crate) fn note(&mut self, why: Refused) {
        self.stats.note(why);
    }

    // ---- what cells name -------------------------------------------------------------------

    /// Lets go of what `cell` names.
    fn release(&mut self, cell: Cell) {
        self.styles.release(cell.style_id(), 1);
        if cell.ext() != 0 {
            if cell.has_hyperlink() {
                let link = self.ext.link(cell.ext());
                self.links.release(link);
            }
            self.ext.give(cell.ext());
        }
    }

    /// The cell an erase leaves (ghostty's `blankCell`): the cursor's background, if its style
    /// has one, else empty. Holds `n` references on the background's style.
    fn blank(&mut self, n: usize) -> Cell {
        if self.cursor.style_id == 0 || self.cursor.style.bg == Color::None || n == 0 {
            return Cell::BLANK;
        }
        let bg = Style { bg: self.cursor.style.bg, ..Style::default() };
        match self.styles.acquire(&bg, n as u32) {
            Ok(id) => Cell::bg(id),
            Err(why) => {
                self.note(why);
                Cell::BLANK
            }
        }
    }

    /// Clears cells `x0..x1` of row `r` to the blank cell (ghostty's `clearCells`).
    pub fn clear_cells(&mut self, r: usize, x0: usize, x1: usize) {
        if x0 >= x1 {
            return;
        }
        let blank = self.blank(x1 - x0);
        for x in x0..x1 {
            let old = self.pages.cell(r, x);
            self.release(old);
            self.pages.set_cell(r, x, blank);
        }
        if x0 == 0 && x1 == self.cols() {
            let row = self.pages.row_mut(r);
            row.kitty_placeholder = false;
        }
    }

    /// Clears to empty cells, not the cursor's background (ghostty's page `clearCells`, a
    /// resize's and an erased row's zeroing).
    fn zero_cells(&mut self, r: usize, x0: usize, x1: usize) {
        for x in x0..x1 {
            let old = self.pages.cell(r, x);
            self.release(old);
            self.pages.set_cell(r, x, Cell::BLANK);
        }
    }

    /// Clears the unprotected cells of `x0..x1` (ghostty's `clearUnprotectedCells`).
    pub fn clear_unprotected_cells(&mut self, r: usize, x0: usize, x1: usize) {
        let mut a = x0;
        while a < x1 {
            while a < x1 && self.pages.cell(r, a).protected() {
                a += 1;
            }
            if a >= x1 {
                break;
            }
            let mut b = a + 1;
            while b < x1 && !self.pages.cell(r, b).protected() {
                b += 1;
            }
            self.clear_cells(r, a, b);
            a = b;
        }
    }

    /// Clears active rows `y0..=y1` (ghostty's `clearRows`): unprotected cells only, keeping
    /// the rows' flags, or every cell with the flags reset.
    pub fn clear_rows(&mut self, y0: usize, y1: usize, protected: bool) {
        let cols = self.cols();
        for y in y0..=y1 {
            let r = self.r(y);
            if protected {
                self.clear_unprotected_cells(r, 0, cols);
            } else {
                self.clear_cells(r, 0, cols);
                self.pages.row_mut(r).reset();
            }
            self.pages.row_mut(r).dirty = true;
        }
    }

    /// Clears what would split a wide character at the boundary left of column `x` of the
    /// cursor's row (ghostty's `splitCellBoundary`, Screen.zig:1732).
    pub fn split_cell_boundary(&mut self, x: usize) {
        let cols = self.cols();
        let r = self.cursor_r();
        if x == cols {
            if !self.pages.row(r).wrap {
                return;
            }
            if self.pages.cell(r, cols - 1).wide() == Wide::SpacerHead {
                self.clear_cells(r, cols - 1, cols);
            }
            return;
        }
        if (x == 0 || x == 1)
            && self.pages.row(r).wrap_continuation
            && self.pages.cell(r, 0).wide() == Wide::Wide
            && r > 0
            && self.pages.cell(r - 1, cols - 1).wide() == Wide::SpacerHead
        {
            self.clear_cells(r - 1, cols - 1, cols);
        }
        if x == 0 {
            return;
        }
        if self.pages.cell(r, x - 1).wide() == Wide::Wide {
            self.clear_cells(r, x - 1, (x + 1).min(cols));
        }
    }

    /// The cursor's row no longer soft-wraps (ghostty's `cursorResetWrap`).
    pub fn cursor_reset_wrap(&mut self) {
        self.cursor.pending_wrap = false;
        let r = self.cursor_r();
        if !self.pages.row(r).wrap {
            return;
        }
        self.pages.row_mut(r).wrap = false;
        if r + 1 < self.pages.total() {
            self.pages.row_mut(r + 1).wrap_continuation = false;
        }
        let cols = self.cols();
        if self.pages.cell(r, cols - 1).wide() == Wide::SpacerHead {
            self.clear_cells(r, cols - 1, cols);
        }
    }

    /// The cursor takes `style`.
    pub fn set_cursor_style(&mut self, style: Style) {
        let id = match self.styles.acquire(&style, 1) {
            Ok(id) => id,
            Err(why) => {
                self.note(why);
                self.styles.release(self.cursor.style_id, 1);
                self.cursor.style = Style::default();
                self.cursor.style_id = 0;
                return;
            }
        };
        self.styles.release(self.cursor.style_id, 1);
        self.cursor.style = style;
        self.cursor.style_id = id;
    }

    /// Writes `cp` (before the charset) at the cursor as a `wide` cell (ghostty's `printCell`,
    /// Terminal.zig:1242).
    pub fn print_cell(&mut self, cp: u32, wide: Wide) {
        let key = self.charset.single_shift.take().unwrap_or(self.charset.gl);
        let c = map_charset(self.charset.sets[key as usize], cp);
        let cols = self.cols();
        let (x, y) = (self.cursor.x, self.cursor.y);
        let r = self.r(y);
        let cell = self.pages.cell(r, x);
        if cell.wide() != wide {
            match cell.wide() {
                Wide::Narrow | Wide::SpacerHead => {}
                Wide::Wide => {
                    if x < cols - 1 {
                        self.clear_cells(r, x + 1, x + 2);
                        self.unhead_prev(y, x);
                    }
                }
                Wide::SpacerTail => {
                    self.clear_cells(r, x - 1, x);
                    self.unhead_prev(y, x);
                }
            }
        }
        let old = self.pages.cell(r, x);
        if old.ext() != 0 {
            if old.has_hyperlink() {
                let link = self.ext.link(old.ext());
                self.links.release(link);
            }
            self.ext.give(old.ext());
        }
        let style = self.cursor.style_id;
        if old.style_id() != style {
            self.styles.release(old.style_id(), 1);
            self.styles.add(style, 1);
        } else if self.plant.has(VtPlant::StyleFreedWhileNamed) && self.styles.refs(style) > 1 {
            // An undercount, never a free on its own: a later release frees it while named.
            self.styles.release(style, 1);
        }
        let mut new = Cell::new(c, style, wide);
        new.set_protected(self.cursor.protected);
        new.set_semantic(self.cursor.semantic);
        if self.cursor.hyperlink != 0 {
            match self.ext.take() {
                Ok(id) => {
                    self.ext.entries[id as usize].link = self.cursor.hyperlink;
                    self.links.add(self.cursor.hyperlink);
                    new.set_ext(id);
                    new.set_hyperlink(true);
                }
                Err(why) => self.note(why),
            }
        }
        self.pages.set_cell(r, x, new);
        if c == 0x10EEEE {
            self.pages.row_mut(r).kitty_placeholder = true;
        }
    }

    /// A wide character that wrapped from the row above leaves a spacer head there; writing
    /// over its first cells drops it (Terminal.zig:1288).
    fn unhead_prev(&mut self, y: usize, x: usize) {
        if y > 0 && x <= 1 {
            let cols = self.cols();
            let pr = self.r(y - 1);
            let mut head = self.pages.cell(pr, cols - 1);
            if head.wide() == Wide::SpacerHead {
                head.set_wide(Wide::Narrow);
                self.pages.set_cell(pr, cols - 1, head);
            }
        }
    }

    /// Appends `cp` to the grapheme of cell `x` of row `r` (ghostty's `appendGrapheme`).
    pub fn append_grapheme(&mut self, r: usize, x: usize, cp: u32) {
        let mut cell = self.pages.cell(r, x);
        if cell.ext() == 0 {
            match self.ext.take() {
                Ok(id) => cell.set_ext(id),
                Err(why) => {
                    self.note(why);
                    return;
                }
            }
        }
        match self.ext.push_cp(cell.ext(), cp) {
            Ok(()) => cell.set_grapheme(true),
            Err(why) => self.note(why),
        }
        self.pages.set_cell(r, x, cell);
        self.pages.row_mut(r).dirty = true;
        if let Err(f) = self.ext.compact() {
            self.note(Refused::Alloc(f));
        }
    }

    /// Drops the grapheme codepoints after the first of cell `x` of row `r`, keeping its link.
    pub fn drop_graphemes(&mut self, r: usize, x: usize) {
        let mut c = self.pages.cell(r, x);
        if !c.has_grapheme() {
            return;
        }
        let id = c.ext();
        self.ext.clear_graphemes(id);
        c.set_grapheme(false);
        if !c.has_hyperlink() {
            self.ext.give(id);
            c.set_ext(0);
        }
        self.pages.set_cell(r, x, c);
    }

    /// Moves the grapheme codepoints (not the first) of cell `(r0, x0)` to cell `(r1, x1)`,
    /// which has none.
    pub fn move_graphemes(&mut self, r0: usize, x0: usize, r1: usize, x1: usize) {
        let mut from = self.pages.cell(r0, x0);
        if !from.has_grapheme() {
            return;
        }
        let mut to = self.pages.cell(r1, x1);
        let id = from.ext();
        if from.has_hyperlink() {
            // The entry also names the source's link: copy the codepoints instead.
            let cps: [u32; 32] = {
                let mut buf = [0u32; 32];
                let g = self.ext.graphemes(id);
                for (i, &c) in g.iter().take(32).enumerate() {
                    buf[i] = c;
                }
                buf
            };
            let n = self.ext.graphemes(id).len().min(32);
            self.ext.clear_graphemes(id);
            from.set_grapheme(false);
            self.pages.set_cell(r0, x0, from);
            for &c in &cps[..n] {
                self.append_grapheme(r1, x1, c);
            }
            return;
        }
        from.set_ext(0);
        from.set_grapheme(false);
        self.pages.set_cell(r0, x0, from);
        if to.ext() != 0 {
            // `to` holds a link entry: move the link into the grapheme entry.
            let link_id = self.ext.link(to.ext());
            self.ext.give(to.ext());
            self.ext.entries[id as usize].link = link_id;
        }
        to.set_ext(id);
        to.set_grapheme(true);
        self.pages.set_cell(r1, x1, to);
    }

    // ---- scrolling -------------------------------------------------------------------------

    /// Fills page-list row `r` with the blank cell (a new row under a styled cursor).
    fn fill_blank(&mut self, r: usize) {
        let cols = self.cols();
        let blank = self.blank(cols);
        if blank != Cell::BLANK {
            for x in 0..cols {
                let old = self.pages.cell(r, x);
                self.release(old);
                self.pages.set_cell(r, x, blank);
            }
        }
    }

    /// Pushes a blank row (the page list's `grow`), first releasing what the rows a prune
    /// would drop name. Returns the rows pruned from the front.
    fn grow(&mut self) -> usize {
        let n = self.pages.grow_would_prune();
        let cols = self.cols();
        for r in 0..n {
            self.zero_cells(r, 0, cols);
        }
        match self.pages.grow() {
            Ok(g) => {
                if g.pruned > 0 {
                    self.pins_shift(g.pruned);
                }
                g.pruned
            }
            Err(f) => {
                self.note(Refused::Alloc(f));
                0
            }
        }
    }

    /// At the bottom row, scrolls the screen up one row (ghostty's `cursorDownScroll`): the
    /// top row goes into scrollback, or, with none, is dropped.
    pub fn cursor_down_scroll(&mut self) {
        let rows = self.rows();
        let cols = self.cols();
        if self.no_scrollback {
            if rows == 1 {
                let r = self.cursor_r();
                self.clear_cells(r, 0, cols);
                self.mark_cursor_dirty();
            } else {
                let top = self.r(0);
                self.zero_cells(top, 0, cols);
                self.pages.rotate_up(top, top + rows - 1);
                // ghostty erases the top row: the pins below move up with their rows.
                for p in self.pins.iter_mut() {
                    if p.r != LOST && p.r > top && p.r < top + rows {
                        p.r -= 1;
                    }
                }
                for y in 0..rows {
                    let r = self.r(y);
                    self.pages.row_mut(r).dirty = true;
                }
            }
        } else {
            self.grow();
            self.mark_cursor_dirty();
        }
        let r = self.cursor_r();
        self.fill_blank(r);
    }

    /// The cursor's row and those above it scroll up one row into scrollback, the rows below
    /// stay (ghostty's `cursorScrollAbove`, Screen.zig:936).
    pub fn cursor_scroll_above(&mut self) {
        self.mark_cursor_dirty();
        if self.cursor.y == self.rows() - 1 {
            self.cursor_down_scroll();
            return;
        }
        self.grow();
        let r = self.cursor_r();
        let last = self.pages.total() - 1;
        self.pages.rotate_down(r, last);
        for rr in r..=last {
            self.pages.row_mut(rr).dirty = true;
        }
        let cols = self.cols();
        self.clear_cells(r, 0, cols);
    }

    /// Scrolls active rows `cursor.y - limit ..= cursor.y` up one (ghostty's
    /// `cursorScrollRegionUp`): the top row is cleared and becomes the cursor's.
    pub fn cursor_scroll_region_up(&mut self, limit: usize) {
        let cols = self.cols();
        let bottom = self.cursor_r();
        let top = bottom - limit;
        self.clear_cells(top, 0, cols);
        self.pages.rotate_up(top, bottom);
        for r in top..=bottom {
            self.pages.row_mut(r).dirty = true;
        }
        // The tracked pins in the region move up with their rows (Screen.zig:1183).
        for p in self.pins.iter_mut() {
            if p.r != LOST && p.r >= top && p.r <= bottom {
                if p.r == 0 {
                    p.x = 0;
                } else {
                    p.r -= 1;
                }
            }
        }
    }

    /// Erases the scrollback (ED 3, ghostty's `eraseHistory`).
    pub fn erase_history(&mut self) {
        let n = self.pages.scrollback();
        let cols = self.cols();
        for r in 0..n {
            self.zero_cells(r, 0, cols);
        }
        self.pages.erase_front(n);
        self.pins_shift(n);
    }

    /// Pushes the active area up to its last non-empty row into scrollback (ED 22, ghostty's
    /// `scrollClear`, PageList.zig:3078). The cursor stays on its row, or goes to the top left
    /// if that row left the active area.
    pub fn scroll_clear(&mut self) {
        let rows = self.rows();
        // Trailing empty rows of the active area.
        let mut empty = 0;
        while empty < rows {
            let r = self.r(rows - 1 - empty);
            if self.pages.cells(r).iter().any(|c| !c.is_empty()) {
                break;
            }
            empty += 1;
        }
        let non_empty = rows - empty;
        let mut cursor_r = self.cursor_r();
        let mut lost = false;
        for _ in 0..non_empty {
            let pruned = self.grow();
            if pruned > cursor_r {
                lost = true;
            }
            cursor_r = cursor_r.saturating_sub(pruned);
        }
        let top = self.pages.active_top();
        if lost || cursor_r < top {
            self.cursor.x = 0;
            self.cursor.y = 0;
        } else {
            self.cursor.y = cursor_r - top;
        }
        self.all_dirty = true;
        self.pages.mark_all_dirty();
    }

    // ---- reset and copy --------------------------------------------------------------------

    /// RIS for this screen (ghostty's `Screen.reset`): blank rows, no scrollback, the cursor
    /// home, nothing saved.
    pub fn reset(&mut self) {
        let total = self.pages.total();
        let cols = self.cols();
        for r in 0..total {
            self.zero_cells(r, 0, cols);
        }
        self.styles.release(self.cursor.style_id, 1);
        self.links.release(self.cursor.hyperlink);
        if let Err(f) = self.pages.reset() {
            self.note(Refused::Alloc(f));
        }
        let shape = self.cursor.shape;
        self.cursor = Cursor { shape, ..Cursor::HOME };
        self.saved = None;
        self.charset = Charsets::default();
        self.kitty = KittyFlagStack::default();
        self.protected_mode = ProtectedMode::Off;
        self.semantic_prompt = SemanticPromptState::default();
        for p in self.pins.iter_mut() {
            p.r = LOST;
        }
        self.all_dirty = true;
    }

    /// Takes `other`'s cursor (its position, style, flags) but not its hyperlink (ghostty's
    /// `cursorCopy` with `hyperlink = false`).
    pub fn copy_cursor(&mut self, other: &Cursor) {
        self.end_hyperlink();
        let style = other.style;
        self.cursor = Cursor {
            style_id: self.cursor.style_id,
            style: self.cursor.style,
            hyperlink: 0,
            ..*other
        };
        self.set_cursor_style(style);
        self.cursor.x = other.x.min(self.cols() - 1);
        self.cursor.y = other.y.min(self.rows() - 1);
    }

    /// Opens a hyperlink at the cursor (OSC 8).
    pub fn start_hyperlink(&mut self, uri: &[u8], id: Option<&[u8]>) {
        self.end_hyperlink();
        match self.links.open(uri, id) {
            Ok(link) => self.cursor.hyperlink = link,
            Err(why) => self.note(why),
        }
    }

    /// Closes the cursor's hyperlink.
    pub fn end_hyperlink(&mut self) {
        self.links.release(self.cursor.hyperlink);
        self.cursor.hyperlink = 0;
    }

    /// What the cursor writes from now on, and the cursor row's prompt mark (OSC 133; ghostty's
    /// `cursorSetSemanticContent`, Screen.zig:2593).
    pub fn set_semantic_content(&mut self, to: SemanticSet) {
        let (content, clear_eol) = match to {
            SemanticSet::Output => (SemanticContent::Output, false),
            SemanticSet::Input { clear_eol } => (SemanticContent::Input, clear_eol),
            SemanticSet::Prompt { continuation } => {
                self.semantic_prompt.seen = true;
                let r = self.cursor_r();
                self.pages.row_mut(r).prompt =
                    if continuation { RowPrompt::Continuation } else { RowPrompt::Prompt };
                (SemanticContent::Prompt, false)
            }
        };
        self.cursor.semantic = content;
        self.cursor.semantic_clear_eol = clear_eol;
    }

    // ---- resize ----------------------------------------------------------------------------

    /// Resizes (ghostty's `Screen.resize`, Screen.zig:1863): reflowing soft wraps when asked,
    /// keeping the cursor and the saved cursor on their cells.
    pub fn resize(&mut self, opts: Resize) {
        if opts.cols == self.cols() && opts.rows == self.rows() {
            return;
        }
        if opts.prompt_redraw != PromptRedraw::False
            && self.cursor.semantic != SemanticContent::Output
        {
            self.clear_prompt_for_redraw(opts.prompt_redraw);
        }
        // The cursor, the saved cursor, then the tracked pins.
        let mut pins: Vec<Pin, Alloc<'a>> = Vec::new_in(self.alloc);
        if let Err(f) = pins.try_reserve(2 + self.pins.len()) {
            self.note(Refused::Alloc(f));
            for p in self.pins.iter_mut() {
                p.r = LOST;
            }
            if pins.try_reserve(2).is_err() {
                return;
            }
        }
        pins.push(Pin { r: self.cursor_r(), x: self.cursor.x, moved: false });
        pins.push(Pin { r: usize::MAX, x: 0, moved: false });
        if let Some(sc) = self.saved
            && sc.x < self.cols()
            && sc.y < self.rows()
        {
            pins[1] = Pin { r: self.r(sc.y), x: sc.x, moved: false };
        }
        if pins.capacity() >= 2 + self.pins.len() {
            for p in self.pins.iter() {
                let r = if p.r == LOST { usize::MAX } else { p.r };
                pins.push(Pin { r, x: p.x, moved: false });
            }
        }
        let pins = &mut pins[..];
        let cursor_y = self.cursor.y;
        if !opts.reflow || opts.cols == self.cols() {
            self.resize_without_reflow(opts.cols, opts.rows, pins);
        } else if opts.cols > self.cols() {
            self.resize_cols(opts.cols, cursor_y, pins);
            self.resize_without_reflow(opts.cols, opts.rows, pins);
        } else {
            let cols = self.cols();
            self.resize_without_reflow(cols, opts.rows, pins);
            self.resize_cols(opts.cols, cursor_y, pins);
        }
        if self.no_scrollback {
            let cols = self.cols();
            let n = self.pages.scrollback();
            let shift = n;
            for r in 0..n {
                self.zero_cells(r, 0, cols);
            }
            self.pages.erase_front(n);
            for p in pins.iter_mut() {
                if p.r != usize::MAX {
                    p.r = p.r.checked_sub(shift).unwrap_or(usize::MAX - 1);
                }
            }
        }
        // The cursor from its pin, or the top left if it left the active area.
        let top = self.pages.active_top();
        let c = pins[0];
        if c.r != usize::MAX && c.r != usize::MAX - 1 && c.r >= top && c.r < self.pages.total() {
            self.cursor.y = c.r - top;
            self.cursor.x = c.x.min(self.cols() - 1);
        } else {
            self.cursor.x = 0;
            self.cursor.y = 0;
        }
        if let Some(sc) = self.saved.as_mut() {
            let p = pins[1];
            if p.r != usize::MAX && p.r != usize::MAX - 1 && p.r >= top && p.r < top + opts.rows {
                sc.x = p.x;
                sc.y = p.r - top;
                if sc.pending_wrap && sc.x != opts.cols - 1 {
                    sc.pending_wrap = false;
                    sc.x += 1;
                }
            } else {
                sc.x = 0;
                sc.y = 0;
                sc.pending_wrap = false;
            }
        }
        if pins.len() == 2 + self.pins.len() {
            let total = self.pages.total();
            for (t, p) in self.pins.iter_mut().zip(&pins[2..]) {
                if p.r < total {
                    t.r = p.r;
                    t.x = p.x.min(opts.cols.max(1) - 1);
                } else {
                    t.r = LOST;
                }
            }
        }
        self.all_dirty = true;
        self.pages.mark_all_dirty();
    }

    /// Clears the prompt under the cursor so the shell can redraw it (Screen.zig:1915).
    fn clear_prompt_for_redraw(&mut self, how: PromptRedraw) {
        let cols = self.cols();
        let r = self.cursor_r();
        match how {
            PromptRedraw::False => {}
            PromptRedraw::Last => self.clear_cells(r, 0, cols),
            PromptRedraw::True => {
                // Up to the first row of the prompt: a row marked prompt, from the cursor up.
                let mut start = r;
                loop {
                    if self.pages.row(start).prompt == RowPrompt::Prompt {
                        break;
                    }
                    if start == 0 {
                        return;
                    }
                    start -= 1;
                    if self.pages.row(start).prompt == RowPrompt::None
                        && self.pages.row(start + 1).prompt != RowPrompt::Continuation
                    {
                        start += 1;
                        if self.pages.row(start).prompt == RowPrompt::None {
                            return;
                        }
                        break;
                    }
                }
                for rr in start..self.pages.total() {
                    self.clear_cells(rr, 0, cols);
                }
            }
        }
    }

    /// Resize with no reflow (PageList.zig:2316): columns cut or padded, rows trimmed from the
    /// blank bottom or pushed into scrollback, or added below (keeping the cursor's row when it
    /// is not at the bottom) or pulled down from scrollback.
    fn resize_without_reflow(&mut self, cols: usize, rows: usize, pins: &mut [Pin]) {
        let old_cols = self.cols();
        if cols != old_cols {
            self.rebuild_cols(cols);
            for p in pins.iter_mut() {
                if p.x >= cols {
                    p.x = cols - 1;
                }
            }
        }
        let old_rows = self.rows();
        if rows < old_rows {
            // Trim trailing blank rows first (rows with no text and no pin).
            let mut trimmed = 0;
            let want = old_rows - rows;
            while trimmed < want {
                let r = self.pages.total() - 1;
                if self.pages.total() <= 1 {
                    break;
                }
                if self.pages.cells(r).iter().any(|c| c.has_text()) {
                    break;
                }
                if pins.iter().any(|p| p.r == r) {
                    break;
                }
                let cols = self.cols();
                self.zero_cells(r, 0, cols);
                self.pages.truncate_back(1);
                trimmed += 1;
            }
            if self.pages.total() < rows {
                self.pages.set_rows_unchecked(rows);
                while self.pages.total() < rows {
                    self.grow_tracking(pins);
                }
            } else {
                self.pages.set_rows(rows);
            }
        } else if rows > old_rows {
            let cursor_y = pins[0].r.wrapping_sub(self.pages.active_top());
            if cursor_y < old_rows - 1 {
                self.pages.set_rows_unchecked(rows);
                // The new rows come below; the active area's top keeps its row.
                for _ in 0..rows - old_rows {
                    self.grow_tracking(pins);
                }
            } else {
                self.pages.set_rows_unchecked(rows);
                while self.pages.total() < rows {
                    self.grow_tracking(pins);
                }
            }
        }
    }

    /// Grows by a row, keeping pins on their rows through a prune.
    fn grow_tracking(&mut self, pins: &mut [Pin]) {
        let pruned = self.grow();
        if pruned > 0 {
            for p in pins.iter_mut() {
                if p.r != usize::MAX {
                    p.r = p.r.checked_sub(pruned).unwrap_or(usize::MAX - 1);
                }
            }
        }
    }

    /// The page list rebuilt at `cols` columns, every row's cells cut or padded, flags kept.
    fn rebuild_cols(&mut self, cols: usize) {
        let old_cols = self.cols();
        let total = self.pages.total();
        let mut list = self.pages.empty_like(cols, self.rows());
        for r in 0..total {
            if let Err(f) = list.grow_unpruned() {
                self.note(Refused::Alloc(f));
                return;
            }
            if cols < old_cols {
                self.zero_cells(r, cols, old_cols);
            }
            let n = cols.min(old_cols);
            let row = *self.pages.row(r);
            let (dst_row, dst) = list.row_and_cells_mut(r);
            dst[..n].copy_from_slice(&self.pages.cells(r)[..n]);
            copy_row_flags(dst_row, &row);
        }
        self.pages = list;
    }

    /// Reflow to `cols` columns (ghostty's `resizeCols`, PageList.zig:1294).
    fn resize_cols(&mut self, cols: usize, cursor_y: usize, pins: &mut [Pin]) {
        let rows = self.rows();
        // The wraps above the cursor in the active area, before.
        let (remaining, wrapped_before) = {
            let c = pins[0];
            let top = self.pages.active_top();
            let wrapped = if c.r >= top && c.r < self.pages.total() {
                (top..=c.r).filter(|&r| self.pages.row(r).wrap_continuation).count()
            } else {
                0
            };
            (rows.saturating_sub(cursor_y + 1), wrapped)
        };
        let old_cols = self.cols();
        let total = self.pages.total();
        let mut dst = Reflow {
            list: self.pages.empty_like(cols, rows),
            x: 0,
            y: 0,
            pending_wrap: false,
            new_rows: 0,
        };
        if let Err(f) = dst.list.grow_unpruned() {
            self.note(Refused::Alloc(f));
            return;
        }
        let drop_tail = self.plant.has(VtPlant::ReflowDropsWrappedTail);
        for r in 0..total {
            let src_row = *self.pages.row(r);
            let cells_at = |s: &Self, x: usize| s.pages.cell(r, x);
            let mut cols_len = old_cols;
            if !src_row.wrap {
                while cols_len > 0 && cells_at(self, cols_len - 1).is_empty() {
                    cols_len -= 1;
                }
                if cols_len == 0 && src_row.prompt != RowPrompt::None {
                    cols_len = 1;
                }
            } else if drop_tail {
                cols_len -= 1;
            }
            // The saved cursor's pin, in blanks past the end, is pulled in.
            {
                let p = &mut pins[1];
                if p.r == r && !p.moved {
                    if p.x >= cols_len {
                        p.x = p.x.min(cols - 1 - dst.x);
                    }
                    cols_len = cols_len.max(p.x + 1);
                }
            }
            if pins[0].r == r && !pins[0].moved {
                cols_len = cols_len.max(pins[0].x + 1);
            }
            if cols_len == 0 {
                if !src_row.wrap_continuation {
                    dst.new_rows += 1;
                }
                self.zero_cells(r, 0, old_cols);
                continue;
            }
            while dst.new_rows > 0 {
                if let Err(f) = dst.scroll() {
                    self.note(Refused::Alloc(f));
                    return;
                }
                dst.new_rows -= 1;
            }
            dst.row_mut().prompt = src_row.prompt;
            let mut x = 0;
            while x < cols_len {
                if dst.pending_wrap {
                    dst.row_mut().wrap = true;
                    if let Err(f) = dst.scroll() {
                        self.note(Refused::Alloc(f));
                        return;
                    }
                    dst.row_mut().prompt = src_row.prompt;
                    dst.row_mut().wrap_continuation = true;
                }
                for p in pins.iter_mut() {
                    if p.r == r && p.x == x && !p.moved {
                        *p = Pin { r: dst.y, x: dst.x, moved: true };
                    }
                }
                let cell = self.pages.cell(r, x);
                match dst.write(cell) {
                    Wrote::Success => x += 1,
                    Wrote::Dropped => {
                        self.release(cell);
                        x += 1;
                    }
                    Wrote::SkipNext => {
                        for p in pins.iter_mut() {
                            if p.r == r && p.x == x + 1 && !p.moved {
                                *p = Pin { r: dst.y, x: dst.x, moved: true };
                            }
                        }
                        // The wide cell's grapheme, hyperlink: gone with its text.
                        let tail =
                            if x + 1 < old_cols { self.pages.cell(r, x + 1) } else { Cell::BLANK };
                        self.release_ext_only(cell);
                        self.release(tail);
                        x += 2;
                    }
                    Wrote::Repeat => {}
                }
            }
            // What was past the trimmed end is empty, but may still name a style.
            for xx in cols_len.min(old_cols)..old_cols {
                let c = self.pages.cell(r, xx);
                self.release(c);
            }
            if !src_row.wrap {
                dst.new_rows += 1;
            }
        }
        // Pins left on rows that wrote nothing go to the end of what was written.
        let new_total = dst.list.total();
        for p in pins.iter_mut() {
            if p.r != usize::MAX && !p.moved {
                *p = Pin { r: new_total - 1, x: p.x.min(cols - 1), moved: true };
            }
        }
        self.pages = dst.list;
        while self.pages.total() < rows {
            if let Err(f) = self.pages.grow_unpruned() {
                self.note(Refused::Alloc(f));
                return;
            }
        }
        // Prune what the cap no longer allows, oldest pages first.
        let prune = self.pages.prunable_front_rows();
        if prune > 0 {
            for r in 0..prune {
                self.zero_cells(r, 0, cols);
            }
            self.pages.erase_front(prune);
            for p in pins.iter_mut() {
                p.r = p.r.checked_sub(prune).unwrap_or(usize::MAX - 1);
            }
        }
        // Keep the rows below the cursor (PageList.zig:1450).
        let c = pins[0];
        let top = self.pages.active_top();
        if c.r != usize::MAX - 1 && c.r >= top && c.r < self.pages.total() {
            let wrapped_after =
                (top..=c.r).filter(|&r| self.pages.row(r).wrap_continuation).count();
            let current = rows.saturating_sub(c.r - top + 1);
            let mut req = remaining;
            req = req.saturating_sub(wrapped_after.saturating_sub(wrapped_before));
            req = req.saturating_sub(current);
            for _ in 0..req {
                self.grow_tracking(pins);
            }
        }
    }

    /// Lets go of a cell's ext entry (graphemes, hyperlink) but not its style.
    fn release_ext_only(&mut self, cell: Cell) {
        if cell.ext() != 0 {
            if cell.has_hyperlink() {
                let link = self.ext.link(cell.ext());
                self.links.release(link);
            }
            self.ext.give(cell.ext());
        }
    }

    /// The text of active row `y`, trailing blanks trimmed, each cell's codepoints.
    pub fn row_text(&self, r: usize, out: &mut impl core::fmt::Write) -> core::fmt::Result {
        let cells = self.pages.cells(r);
        let mut end = cells.len();
        while end > 0 && !cells[end - 1].has_text() {
            end -= 1;
        }
        for &c in &cells[..end] {
            match c.wide() {
                Wide::SpacerTail | Wide::SpacerHead => continue,
                _ => {}
            }
            if c.codepoint() == 0 {
                out.write_char(' ')?;
                continue;
            }
            out.write_char(char::from_u32(c.codepoint()).unwrap_or('\u{fffd}'))?;
            for &g in self.graphemes(c) {
                out.write_char(char::from_u32(g).unwrap_or('\u{fffd}'))?;
            }
        }
        Ok(())
    }

    /// The allocator the screen's tables are in.
    pub fn allocator(&self) -> Alloc<'a> {
        self.alloc
    }
}

fn copy_row_flags(dst: &mut Row, src: &Row) {
    dst.wrap = src.wrap;
    dst.wrap_continuation = src.wrap_continuation;
    dst.prompt = src.prompt;
    dst.kitty_placeholder = src.kitty_placeholder;
    dst.dirty = true;
}

/// What a reflow write did with a source cell.
enum Wrote {
    Success,
    /// Not written (a spacer the new width does not need); its references are the caller's to
    /// let go.
    Dropped,
    /// Written, and the next source cell (a spacer tail) is skipped.
    SkipNext,
    /// Not written yet: a spacer head was put first; write the same cell again.
    Repeat,
}

/// The write position of a reflow (ghostty's `ReflowCursor`).
struct Reflow<'a, P> {
    list: PageList<'a, P>,
    x: usize,
    /// The page-list row written.
    y: usize,
    pending_wrap: bool,
    new_rows: usize,
}

impl<P: Planted<VtPlant>> Reflow<'_, P> {
    fn row_mut(&mut self) -> &mut Row {
        self.list.row_mut(self.y)
    }

    fn scroll(&mut self) -> Result<(), AllocFailure> {
        self.list.grow_unpruned()?;
        self.y += 1;
        self.x = 0;
        self.pending_wrap = false;
        Ok(())
    }

    fn forward(&mut self) {
        if self.x == self.list.cols() - 1 {
            self.pending_wrap = true;
        } else {
            self.x += 1;
        }
    }

    fn write(&mut self, cell: Cell) -> Wrote {
        let cols = self.list.cols();
        if cell.is_bg() {
            self.list.set_cell(self.y, self.x, cell);
            self.forward();
            return Wrote::Success;
        }
        match cell.wide() {
            Wide::Narrow => self.list.set_cell(self.y, self.x, cell),
            Wide::Wide => {
                if cols > 1 {
                    if self.x == cols - 1 {
                        self.list.set_cell(self.y, self.x, Cell::new(0, 0, Wide::SpacerHead));
                        self.forward();
                        return Wrote::Repeat;
                    }
                    self.list.set_cell(self.y, self.x, cell);
                } else {
                    let c = Cell::new(0, cell.style_id(), Wide::Narrow);
                    self.list.set_cell(self.y, self.x, c);
                    self.forward();
                    return Wrote::SkipNext;
                }
            }
            Wide::SpacerTail => {
                if cols > 1 {
                    self.list.set_cell(self.y, self.x, cell);
                } else {
                    return Wrote::Dropped;
                }
            }
            Wide::SpacerHead => return Wrote::Dropped,
        }
        if cell.codepoint() == 0x10EEEE {
            self.row_mut().kitty_placeholder = true;
        }
        self.forward();
        Wrote::Success
    }
}
