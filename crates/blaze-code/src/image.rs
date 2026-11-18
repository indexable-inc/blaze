//! Segments and the program image (run.md, "End state", "The segment" and "The image").
//!
//! - [`lay_out`] places sealed code records together as one [`Segment`]: their code at the
//!   alignment each record asks (`CodeView::code_align`, at least 16) in the order given, then, on the next page, the import table (one word per distinct target)
//!   and the records' relro parts. It completes every site's instructions against that layout.
//!   A segment's bytes do not depend on where it is mapped: `adrp` reaches by 4 KiB page
//!   difference and every segment starts on a page. Equal records are placed once (folding at
//!   record grain). In a [`Kind::Bundle`] a call within the segment becomes a direct `bl`; a
//!   [`Kind::Delta`] segment reaches everything through import words, so a later edit never
//!   writes its code.
//! - [`Image`] gives each appended segment the next free address and binds its words. It keeps
//!   where every target lives and, for each target, the words that name it (postings). A
//!   segment that places a function an earlier one placed (an edit) is a move: the words naming
//!   it are rewritten, and nothing else is ([`Appended::writes`]). A grown body is placed new,
//!   so nothing needs slack and nothing beside it moves.

use core::fmt;

use blaze_ids::Hasher;
use blaze_own::{ABTreeMap, AVec, Alloc, BTreeMap, Vec};

use crate::a64::{ADD64, ADRP, BLR, LDR64, NOP, add_lo12, adrp, bl, expect, ldr_lo12, up, write32};
use crate::record::{CodeView, OWN_CODE, OWN_RELRO, PositionRow, SiteKind, Space, Symbol, Target};

const FOLD_DOMAIN: &str = "blaze-code fold 1";

/// Whether a segment's records may be replaced one by one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A dependency bundle: replaced only whole, so a call to a function of the same segment is
    /// a direct `bl`, and a load of such a function's address an `adrp; add`.
    Bundle,
    /// One command's records: any may be replaced by a later segment, so every reference goes
    /// through an import word.
    Delta,
}

/// A function placed in a segment: its symbol and its record.
#[derive(Clone, Copy)]
pub struct Member<'r> {
    pub symbol: Symbol,
    pub record: CodeView<'r>,
}

/// What a relro word holds the address of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bound {
    /// A target, where the image says it lives (a thread-local: its offset in a thread's block).
    Target(Target),
    /// A byte of this segment, by its offset from the segment's first byte.
    Own(u64),
}

/// A segment laid out, before it has an address.
pub struct Segment<'a> {
    /// The code region: every placed record's code, 16-aligned, its sites completed. What is
    /// mapped read-only and executable.
    pub code: AVec<'a, u8>,
    /// Each member's function and its code's offset from the segment's first byte, in member
    /// order; folded members share an offset.
    pub homes: AVec<'a, (Symbol, u64)>,
    /// The import table's targets, sorted: target `i`'s word is at `imports_at + 8 * i`.
    pub imports: AVec<'a, Target>,
    /// A whole number of pages: the code region's end, rounded up.
    pub imports_at: u64,
    /// The placed records' relro parts, each 16-aligned, every word 0 until the image binds it.
    pub relro: AVec<'a, u8>,
    pub relro_at: u64,
    /// Each relro word: its offset in `relro`, what it holds the address of, and its addend.
    pub words: AVec<'a, (u64, Bound, i64)>,
    /// The address space the segment takes: a whole number of pages.
    pub size: u64,
}

/// Why a segment was not laid out or bound: naming the function and the offset it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageError {
    /// The page size is not a power of two of at least 4 KiB.
    Page { page: u64 },
    /// The image's base is not on a page.
    Base { base: u64 },
    /// One function is placed twice in one segment.
    TwoHomes { symbol: Symbol },
    /// The function's code at segment offset `at` is not the instruction its site says (`want`).
    NotInstruction { symbol: Symbol, at: u64, want: &'static str },
    /// The function's site at segment offset `at` cannot reach its target with `what`.
    OutOfReach { symbol: Symbol, at: u64, what: &'static str },
    /// A word names a target no segment placed and nothing defined.
    Unbound { target: Target },
    /// A relro word names an outside symbol with an addend: `dlsym` binds a name, not a name
    /// plus an offset.
    OutsideAddend { symbol: Symbol },
    /// A function is given a home by [`Image::define`]: only a segment places a function.
    DefinedFunction { symbol: Symbol },
    /// The placer has no current source position for an item-relative field.
    UnresolvedPosition { symbol: Symbol, row: PositionRow },
    /// Source lines and columns both start at one.
    ZeroPosition { symbol: Symbol, row: PositionRow },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ImageError::Page { page } => write!(f, "the page size {page} is not a power of two of at least 4096 (run.md, \"The segment\")"),
            ImageError::Base { base } => write!(f, "the image's base {base:#x} is not on a page"),
            ImageError::TwoHomes { symbol } => write!(f, "function {symbol} is placed twice in one segment"),
            ImageError::NotInstruction { symbol, at, want } => write!(f, "function {symbol}: the code at segment offset {at:#x} is not the {want} its site says"),
            ImageError::OutOfReach { symbol, at, what } => write!(f, "function {symbol}: the site at segment offset {at:#x} cannot reach its target with {what}"),
            ImageError::Unbound { target } => write!(f, "a word names {:?} {}, which no segment placed and nothing defined", target.space, target.symbol),
            ImageError::OutsideAddend { symbol } => write!(f, "a relro word names the outside symbol {symbol} with an addend; a name is bound by `dlsym` alone"),
            ImageError::DefinedFunction { symbol } => write!(f, "function {symbol} was given a home by `define`; only a segment places a function"),
            ImageError::UnresolvedPosition { symbol, row } => write!(
                f, "function {symbol}: no source position for item {:#x} at byte {} ({:?} in {:?} at byte {})",
                row.item, row.site, row.field, row.part, row.at,
            ),
            ImageError::ZeroPosition { symbol, row } => write!(
                f, "function {symbol}: zero {:?} for item {:#x} at byte {} ({:?} at byte {}), source positions start at one",
                row.field, row.item, row.site, row.part, row.at,
            ),
        }
    }
}

impl core::error::Error for ImageError {}

/// Lays `members` out as one segment for a target whose pages are `page` bytes (16 KiB on macOS
/// on arm64; the largest page a Linux arm64 kernel may use, 64 KiB, so one layout serves every
/// kernel). Deterministic: the same members in the same order give the same bytes.
pub fn lay_out<'a>(members: &[Member<'_>], kind: Kind, page: u64, alloc: Alloc<'a>) -> Result<Segment<'a>, ImageError> {
    lay_out_positions(members, kind, page, alloc, |symbol, row| Err(ImageError::UnresolvedPosition { symbol, row }))
}

/// Lays out the segment using the current checkout's source positions. Folded members have
/// equal item keys and sites in their records and therefore share the resolved fields.
pub fn lay_out_positions<'a>(
    members: &[Member<'_>],
    kind: Kind,
    page: u64,
    alloc: Alloc<'a>,
    mut position: impl FnMut(Symbol, PositionRow) -> Result<(u32, u32), ImageError>,
) -> Result<Segment<'a>, ImageError> {
    if page < 4096 || !page.is_power_of_two() {
        return Err(ImageError::Page { page });
    }
    let n = members.len();
    // Folding: each member's representative is the first member with equal bytes.
    let mut digests: AVec<'_, ([u8; 32], u32)> = Vec::with_capacity_in(n, alloc);
    for (i, m) in members.iter().enumerate() {
        digests.push((Hasher::new(FOLD_DOMAIN).update(m.record.bytes()).finish().0, i as u32));
    }
    digests.sort_unstable();
    let mut rep: AVec<'_, u32> = Vec::from_elem_in(0, n, alloc);
    let mut run = 0usize;
    for k in 0..digests.len() {
        let (digest, i) = digests[k];
        if digests[run].0 != digest {
            run = k;
        }
        let first = digests[run].1;
        let equal = members[first as usize].record.bytes() == members[i as usize].record.bytes();
        rep[i as usize] = if equal { first } else { i };
    }
    drop(digests);
    // Place code and relro parts in member order.
    let mut code: AVec<'a, u8> = Vec::new_in(alloc);
    let mut relro: AVec<'a, u8> = Vec::new_in(alloc);
    let mut code_at: AVec<'_, u64> = Vec::from_elem_in(0, n, alloc);
    let mut relro_off: AVec<'_, u64> = Vec::from_elem_in(0, n, alloc);
    let mut homes: AVec<'a, (Symbol, u64)> = Vec::with_capacity_in(n, alloc);
    let mut local: ABTreeMap<'_, Symbol, u64> = BTreeMap::new_in(alloc);
    // The most any placed relro part asks: where the relro region starts.
    let mut most_relro = 16u64;
    for (i, m) in members.iter().enumerate() {
        let r = rep[i] as usize;
        if r == i {
            // Each part at the alignment its record asks (at least 16 and 8, at most a 4 KiB
            // page, which the segment's first byte and `relro_at` give: record.rs `seal`).
            let code_align = 1usize << m.record.code_align();
            while code.len() % code_align != 0 {
                code.push(0);
            }
            code_at[i] = code.len() as u64;
            code.extend_from_slice(m.record.code());
            let relro_align = 16usize.max(1 << m.record.relro_align());
            most_relro = most_relro.max(relro_align as u64);
            while relro.len() % relro_align != 0 {
                relro.push(0);
            }
            relro_off[i] = relro.len() as u64;
            relro.extend_from_slice(m.record.relro());
            crate::place::fill_positions(&m.record, m.symbol, &mut code[code_at[i] as usize..], &mut relro[relro_off[i] as usize..], |_, row| {
                Ok(row.field.value(position(m.symbol, row)?))
            })?;
        } else {
            code_at[i] = code_at[r];
            relro_off[i] = relro_off[r];
        }
        if local.insert(m.symbol, code_at[i]).is_some() {
            return Err(ImageError::TwoHomes { symbol: m.symbol });
        }
        homes.push((m.symbol, code_at[i]));
    }
    // A site is relaxed when it calls or loads a function of this bundle.
    let relaxed = |t: Target| -> Option<u64> {
        match (kind, t.space) {
            (Kind::Bundle, Space::Func) => local.get(&t.symbol).copied(),
            _ => None,
        }
    };
    let mut imports: AVec<'a, Target> = Vec::new_in(alloc);
    for (i, m) in members.iter().enumerate() {
        if rep[i] as usize != i {
            continue;
        }
        for s in 0..m.record.sites() {
            let site = m.record.site(s);
            if site.kind == SiteKind::Relro {
                continue;
            }
            let t = m.record.target(site.target as usize);
            if relaxed(t).is_none() {
                imports.push(t);
            }
        }
    }
    imports.sort_unstable();
    imports.dedup();
    let imports_at = up(code.len() as u64, page);
    let relro_at = up(imports_at + 8 * imports.len() as u64, most_relro);
    let size = up(relro_at + relro.len() as u64, page);
    // Complete every placed record's sites against the layout.
    let mut words: AVec<'a, (u64, Bound, i64)> = Vec::new_in(alloc);
    for (i, m) in members.iter().enumerate() {
        if rep[i] as usize != i {
            continue;
        }
        let symbol = m.symbol;
        for s in 0..m.record.sites() {
            let site = m.record.site(s);
            let pc = code_at[i] + u64::from(site.at);
            if site.kind == SiteKind::Relro {
                let to = relro_at + relro_off[i] + u64::from(site.addend);
                let first = expect(&code, pc, ADRP, symbol, "adrp")?;
                let second = expect(&code, pc + 4, ADD64, symbol, "add")?;
                write32(&mut code, pc, adrp(first, pc, to, symbol)?);
                write32(&mut code, pc + 4, add_lo12(second, to));
                continue;
            }
            let t = m.record.target(site.target as usize);
            let first = expect(&code, pc, ADRP, symbol, "adrp")?;
            let second = expect(&code, pc + 4, LDR64, symbol, "ldr")?;
            if site.kind == SiteKind::Call {
                expect(&code, pc + 8, BLR, symbol, "blr")?;
            }
            match (relaxed(t), site.kind) {
                (Some(home), SiteKind::Call) => {
                    write32(&mut code, pc, NOP);
                    write32(&mut code, pc + 4, NOP);
                    write32(&mut code, pc + 8, bl(pc + 8, home, symbol)?);
                }
                (Some(home), _) => {
                    write32(&mut code, pc, adrp(first, pc, home, symbol)?);
                    write32(&mut code, pc + 4, add_lo12(second, home));
                }
                (None, _) => {
                    let Ok(index) = imports.binary_search(&t) else {
                        unreachable!("every unrelaxed target was put in the import table above");
                    };
                    let word = imports_at + 8 * index as u64;
                    write32(&mut code, pc, adrp(first, pc, word, symbol)?);
                    write32(&mut code, pc + 4, ldr_lo12(second, word));
                }
            }
        }
        for w in 0..m.record.words() {
            let word = m.record.word(w);
            let at = relro_off[i] + u64::from(word.at);
            let bound = match word.target {
                OWN_CODE => Bound::Own(code_at[i]),
                OWN_RELRO => Bound::Own(relro_at + relro_off[i]),
                index => Bound::Target(m.record.target(index as usize)),
            };
            words.push((at, bound, word.addend));
        }
    }
    drop(local);
    Ok(Segment { code, homes, imports, imports_at, relro, relro_at, words, size })
}

/// One word of the image to write: `value` at address `at`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Write {
    pub at: u64,
    pub value: u64,
}

/// What appending a segment asks of the image's memory.
pub struct Appended<'a> {
    /// Where the segment's first byte goes: its code at `base`, its import table and relro part
    /// at `base + imports_at`.
    pub base: u64,
    /// Every word to store, sorted by address, each once: the new segment's bound words, and
    /// in earlier segments exactly the words naming a function this segment moved.
    pub writes: AVec<'a, Write>,
    /// The words the launcher binds at start with `dlsym`, by the name's symbol.
    pub outside: AVec<'a, (u64, Symbol)>,
    /// The functions this segment placed that an earlier segment had placed: an edit's bodies.
    pub moved: AVec<'a, Symbol>,
}

/// A program's image: where each segment and each target is, and which words name each target.
pub struct Image<'a> {
    page: u64,
    next: u64,
    homes: ABTreeMap<'a, Target, u64>,
    postings: ABTreeMap<'a, Target, AVec<'a, (u64, i64)>>,
    alloc: Alloc<'a>,
}

impl<'a> Image<'a> {
    /// An empty image whose first segment goes at `base`.
    pub fn new(base: u64, page: u64, alloc: Alloc<'a>) -> Result<Image<'a>, ImageError> {
        if page < 4096 || !page.is_power_of_two() {
            return Err(ImageError::Page { page });
        }
        if base % page != 0 {
            return Err(ImageError::Base { base });
        }
        Ok(Image { page, next: base, homes: BTreeMap::new_in(alloc), postings: BTreeMap::new_in(alloc), alloc })
    }

    /// Says where a data target lives: a static's or vtable's address, a thread-local's offset.
    pub fn define(&mut self, target: Target, value: u64) -> Result<(), ImageError> {
        if target.space == Space::Func {
            return Err(ImageError::DefinedFunction { symbol: target.symbol });
        }
        self.homes.insert(target, value);
        Ok(())
    }

    /// Where `target` lives now.
    pub fn home(&self, target: Target) -> Option<u64> {
        self.homes.get(&target).copied()
    }

    /// Gives `segment` the next free address and binds it. A function it places that an earlier
    /// segment placed moves here: every word naming it is rewritten, and no other word of an
    /// earlier segment is.
    pub fn append(&mut self, segment: &Segment<'_>) -> Result<Appended<'a>, ImageError> {
        let alloc = self.alloc;
        let base = self.next;
        self.next = base + up(segment.size, self.page);
        let mut moved: AVec<'a, Symbol> = Vec::new_in(alloc);
        for &(symbol, at) in segment.homes.iter() {
            if self.homes.insert(Target { space: Space::Func, symbol }, base + at).is_some() {
                moved.push(symbol);
            }
        }
        moved.sort_unstable();
        moved.dedup();
        let mut writes: AVec<'a, Write> = Vec::new_in(alloc);
        let mut outside: AVec<'a, (u64, Symbol)> = Vec::new_in(alloc);
        // First the earlier words naming a moved function, before this segment's own words are
        // posted, so none is written twice.
        for &symbol in moved.iter() {
            let target = Target { space: Space::Func, symbol };
            let Some(home) = self.homes.get(&target).copied() else {
                unreachable!("a moved function was given its home above");
            };
            if let Some(words) = self.postings.get(&target) {
                for &(at, addend) in words.iter() {
                    writes.push(Write { at, value: home.wrapping_add_signed(addend) });
                }
            }
        }
        for (i, &target) in segment.imports.iter().enumerate() {
            let at = base + segment.imports_at + 8 * i as u64;
            self.bind(target, at, 0, &mut writes, &mut outside)?;
        }
        for &(offset, bound, addend) in segment.words.iter() {
            let at = base + segment.relro_at + offset;
            match bound {
                Bound::Own(own) => writes.push(Write { at, value: (base + own).wrapping_add_signed(addend) }),
                Bound::Target(target) => self.bind(target, at, addend, &mut writes, &mut outside)?,
            }
        }
        writes.sort_unstable_by_key(|w| w.at);
        outside.sort_unstable();
        Ok(Appended { base, writes, outside, moved })
    }

    /// Binds the word at `at` to `target` plus `addend`, and posts it under the target.
    fn bind(&mut self, target: Target, at: u64, addend: i64, writes: &mut AVec<'a, Write>, outside: &mut AVec<'a, (u64, Symbol)>) -> Result<(), ImageError> {
        if target.space == Space::Outside {
            if addend != 0 {
                return Err(ImageError::OutsideAddend { symbol: target.symbol });
            }
            outside.push((at, target.symbol));
            return Ok(());
        }
        let Some(home) = self.homes.get(&target).copied() else {
            return Err(ImageError::Unbound { target });
        };
        writes.push(Write { at, value: home.wrapping_add_signed(addend) });
        let alloc = self.alloc;
        self.postings.get_or_insert_with(&target, |t| (*t, Vec::new_in(alloc))).push((at, addend));
        Ok(())
    }
}
