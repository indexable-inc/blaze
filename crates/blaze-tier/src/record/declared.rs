//! The DECLARED INSTANCES section of the code record (`bzcode03`, decision D106): for each thing
//! the code names inside the program, its symbol as the relocations' targets use it and the data
//! its record's KEY is made from without the body, so `blaze run` follows a found record to what
//! it calls with nothing checked, lowered or named (blaze-ladder body_ids.rs `Walk`).
//!
//! A target's symbol is a digest: no key can be made from it. A row holds exactly the inputs of
//! the callee's key that are the callee's own:
//!
//! | kind | target space | `item` | `name` |
//! |---|---|---|---|
//! | [`DeclaredKind::Instance`] | `Func` | the TABLED fn (whose body or signature builds it) | blaze-ladder symbols.rs `encode_instance` bytes |
//! | [`DeclaredKind::Static`] | `Static` | the static | empty |
//! | [`DeclaredKind::Vtable`] | `Vtable` | the principal trait | `vtable_symbol`'s input after the principal: the concrete type's tree, then the trait's argument trees |
//! | [`DeclaredKind::ThreadLocal`] | `Tls` | the thread-local | empty |
//! | [`DeclaredKind::TlsDescriptor`] | `Static` | 0 | empty (the program's one descriptor) |
//! | [`DeclaredKind::TlsGetter`] | `Func` | 0 | empty (the program's one getter) |
//! | [`DeclaredKind::Bound`] | `Func` | 0 | empty: a symbol only an export's start code binds (`main`, `lang_start`); a walk refuses it by name and never follows it |
//!
//! NOT IN A ROW, supplied by the READER: the code builder's identity (decision D105:
//! `blaze_ids::ProducerId<CodeBuilder>`, handed in by the caller), the target, the tier, the
//! settings and the toolchain (blaze-ladder body_ids.rs `record_key` writes them), and today's
//! facts (the BodyCheck key and read list of `item`, the crate facts of the items `name` holds).
//! Nothing of the machine is in a row (rule 25): two builders of one key write equal bytes.
//!
//! THE RULES (the seal sorts and checks them once; [`super::read`] checks them again on bytes
//! from a store or a fetch, once: rule 74): rows sorted by symbol, each symbol once (two equal
//! rows are one; two different rows of one symbol are refused); every row's kind fits its
//! `item` and `name` as the table says; every row's (space of its kind, symbol) is a target of
//! the record; every target in `Func`, `Static`, `Vtable` or `Tls` has a row of its space. So a
//! walk trusts the section with no check of its own.
//!
//! Row (64 bytes, little-endian): symbol 32, `item: u128` 16, kind `u8`, 3 zero, name `at: u32`,
//! name `len: u32`, 4 zero. The names' bytes are in the record's text, after the C names, in row
//! order; the C names and then the declared names tile the text exactly (the seal rebuilds it
//! so; the read refuses any other layout), so the bytes are canonical.

use core::fmt;

use blaze_code::record::{Space, Symbol, Target};
use blaze_own::{AVec, Alloc, Vec};

use super::Emitted;
use super::form::FormError;

/// A declared row's size in bytes.
pub(super) const ROW_BYTES: usize = 64;

/// What a declared row is, and so how its key is made (the module's table).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum DeclaredKind {
    Instance = 0,
    Static = 1,
    Vtable = 2,
    ThreadLocal = 3,
    TlsDescriptor = 4,
    TlsGetter = 5,
    Bound = 6,
}

impl DeclaredKind {
    /// The kind a row's byte names; `None` for a byte no kind has.
    pub fn of_byte(byte: u8) -> Option<DeclaredKind> {
        Some(match byte {
            0 => DeclaredKind::Instance,
            1 => DeclaredKind::Static,
            2 => DeclaredKind::Vtable,
            3 => DeclaredKind::ThreadLocal,
            4 => DeclaredKind::TlsDescriptor,
            5 => DeclaredKind::TlsGetter,
            6 => DeclaredKind::Bound,
            _ => return None,
        })
    }

    /// The space of the target a row of this kind declares.
    pub fn space(self) -> Space {
        match self {
            DeclaredKind::Instance | DeclaredKind::TlsGetter | DeclaredKind::Bound => Space::Func,
            DeclaredKind::Static | DeclaredKind::TlsDescriptor => Space::Static,
            DeclaredKind::Vtable => Space::Vtable,
            DeclaredKind::ThreadLocal => Space::Tls,
        }
    }

    /// Whether `item` and a name of `name_len` bytes fit this kind (the module's table).
    fn fits(self, item: u128, name_len: u32) -> bool {
        match self {
            DeclaredKind::Instance | DeclaredKind::Vtable => name_len > 0,
            DeclaredKind::Static | DeclaredKind::ThreadLocal => name_len == 0,
            DeclaredKind::TlsDescriptor | DeclaredKind::TlsGetter | DeclaredKind::Bound => item == 0 && name_len == 0,
        }
    }
}

impl fmt::Display for DeclaredKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            DeclaredKind::Instance => "fn instance",
            DeclaredKind::Static => "static",
            DeclaredKind::Vtable => "vtable",
            DeclaredKind::ThreadLocal => "thread-local",
            DeclaredKind::TlsDescriptor => "thread-local descriptor",
            DeclaredKind::TlsGetter => "thread-local getter",
            DeclaredKind::Bound => "start-bound symbol",
        };
        f.write_str(text)
    }
}

/// One row in memory ([`Emitted::declared`]): its name is `text[at..at + len]` of the record.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DeclaredRow {
    pub symbol: Symbol,
    pub kind: DeclaredKind,
    /// The item's key (blaze-typeck `ItemKey`'s `u128`; this crate does not name that type).
    pub item: u128,
    pub at: u32,
    pub len: u32,
}

/// One row as a reader sees it, borrowing its name from the record's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Declared<'b> {
    pub symbol: Symbol,
    pub kind: DeclaredKind,
    pub item: u128,
    pub name: &'b [u8],
}

/// What a producer says of one in-program target: the key data of its row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Declare<'n> {
    pub kind: DeclaredKind,
    pub item: u128,
    pub name: &'n [u8],
}

/// The producer's side (the run path's names of one built instance): the key data of each
/// in-program target the emitted code names.
pub trait Declares {
    /// `target`'s row data (`target.space` is `Func`, `Static`, `Vtable` or `Tls`); `None` where
    /// the producer has none, which [`declare`] refuses by name.
    fn declared(&self, target: Target) -> Option<Declare<'_>>;
}

/// Adds to `e` a row for each of its in-program targets, from `by`, before the seal (which
/// sorts the rows, merges equal ones and checks them). Refuses a target `by` has no row for, or
/// a row whose kind is of another space, by name.
pub fn declare(e: &mut Emitted<'_>, by: &dyn Declares) -> Result<(), FormError> {
    for i in 0..e.targets.len() {
        let target = e.targets[i];
        if target.space == Space::Outside {
            continue;
        }
        let Some(d) = by.declared(target) else {
            return Err(FormError::DeclaredMissing { symbol: target.symbol, space: target.space });
        };
        if d.kind.space() != target.space {
            return Err(FormError::DeclaredSpace { symbol: target.symbol, space: target.space, kind: d.kind });
        }
        let at = e.text.len();
        let (Ok(at32), Ok(len32)) = (u32::try_from(at), u32::try_from(d.name.len())) else {
            return Err(FormError::TooLarge { what: "names' text" });
        };
        if u32::try_from(at + d.name.len()).is_err() {
            return Err(FormError::TooLarge { what: "names' text" });
        }
        e.text.extend_from_slice(d.name);
        e.declared.push(DeclaredRow { symbol: target.symbol, kind: d.kind, item: d.item, at: at32, len: len32 });
    }
    Ok(())
}

/// The seal's part: the rows sorted by symbol, equal rows merged, two different rows of one
/// symbol refused; each name copied from `old` (the text before the seal rebuilt it) to the end
/// of `e.text` (which holds the C names already), in row order. Scratch in `alloc`.
pub(super) fn seal_rows<'a>(e: &mut Emitted<'a>, old: &[u8], alloc: Alloc<'a>) -> Result<(), FormError> {
    let mut rows = core::mem::replace(&mut e.declared, Vec::new_in(alloc));
    for r in rows.iter() {
        if name_in(old, r).is_none() {
            return Err(FormError::DeclaredText { symbol: r.symbol });
        }
    }
    let name = |r: &DeclaredRow| name_in(old, r).unwrap_or(&[]);
    rows.sort_unstable_by(|a, b| (a.symbol, a.kind, a.item, name(a)).cmp(&(b.symbol, b.kind, b.item, name(b))));
    let mut kept: AVec<'a, DeclaredRow> = Vec::with_capacity_in(rows.len(), alloc);
    let mut last: Option<DeclaredRow> = None;
    for &r in rows.iter() {
        if let Some(prev) = last
            && prev.symbol == r.symbol
        {
            if prev.kind == r.kind && prev.item == r.item && name(&prev) == name(&r) {
                continue;
            }
            return Err(FormError::DeclaredTwice { symbol: r.symbol });
        }
        last = Some(r);
        let bytes = name(&r);
        let at = e.text.len();
        if u32::try_from(at + bytes.len()).is_err() {
            return Err(FormError::TooLarge { what: "names' text" });
        }
        e.text.extend_from_slice(bytes);
        kept.push(DeclaredRow { at: at as u32, ..r });
    }
    drop(rows);
    e.declared = kept;
    Ok(())
}

fn name_in<'t>(text: &'t [u8], r: &DeclaredRow) -> Option<&'t [u8]> {
    let from = r.at as usize;
    text.get(from..from.checked_add(r.len as usize)?)
}

/// Appends row `r`'s 64 bytes.
pub(super) fn put_row(out: &mut AVec<'_, u8>, r: &DeclaredRow) {
    out.extend_from_slice(&r.symbol.0);
    out.extend_from_slice(&r.item.to_le_bytes());
    out.extend_from_slice(&[r.kind as u8, 0, 0, 0]);
    out.extend_from_slice(&r.at.to_le_bytes());
    out.extend_from_slice(&r.len.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
}

/// Checks one encoded row's tag and zero bytes: `Err(byte)` names the first wrong byte.
/// `b` is exactly [`ROW_BYTES`] long (the read sliced it so).
pub(super) fn check_row_bytes(b: &[u8]) -> Result<(), u8> {
    if DeclaredKind::of_byte(b[48]).is_none() {
        return Err(b[48]);
    }
    for &z in b[49..52].iter().chain(b[60..64].iter()) {
        if z != 0 {
            return Err(z);
        }
    }
    Ok(())
}

/// Row `b` (checked by [`check_row_bytes`]) in memory.
pub(super) fn row_of_bytes(b: &[u8]) -> DeclaredRow {
    let mut symbol = [0u8; 32];
    symbol.copy_from_slice(&b[0..32]);
    let mut item = [0u8; 16];
    item.copy_from_slice(&b[32..48]);
    let Some(kind) = DeclaredKind::of_byte(b[48]) else {
        unreachable!("read checked every declared row's kind byte");
    };
    let word = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    DeclaredRow { symbol: Symbol(symbol), kind, item: u128::from_le_bytes(item), at: word(52), len: word(56) }
}

/// Row `r` with its name borrowed from `text`; the check proved the range is inside it.
pub(super) fn view_of<'b>(r: DeclaredRow, text: &'b [u8]) -> Declared<'b> {
    let Some(name) = name_in(text, &r) else {
        unreachable!("read checked every declared name's range");
    };
    Declared { symbol: r.symbol, kind: r.kind, item: r.item, name }
}

/// The section's rules (the module's documentation), and the text's layout: the C names
/// (`names` rows, each `(symbol, at, len)`) then the declared names tile `text_len` bytes
/// exactly, in row order. Targets are sorted by (space, symbol) and unique (checked before).
pub(super) fn check(
    declared: usize,
    row: impl Fn(usize) -> DeclaredRow,
    targets: usize,
    target: impl Fn(usize) -> Target,
    names: usize,
    name: impl Fn(usize) -> (Symbol, u32, u32),
    text_len: usize,
) -> Result<(), FormError> {
    let mut pos = 0u64;
    for i in 0..names {
        let (_, at, len) = name(i);
        if u64::from(at) != pos {
            return Err(FormError::TextLayout { section: "names", row: i as u32 });
        }
        pos += u64::from(len);
    }
    for i in 0..declared {
        let r = row(i);
        if u64::from(r.at) != pos {
            return Err(FormError::TextLayout { section: "declared instances", row: i as u32 });
        }
        pos += u64::from(r.len);
    }
    if pos != text_len as u64 {
        return Err(FormError::TextLayout { section: "names' text", row: (names + declared) as u32 });
    }
    let has_target = |t: Target| -> bool {
        let (mut lo, mut hi) = (0usize, targets);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match target(mid).cmp(&t) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return true,
            }
        }
        false
    };
    let row_of = |symbol: Symbol| -> Option<DeclaredRow> {
        let (mut lo, mut hi) = (0usize, declared);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let r = row(mid);
            match r.symbol.cmp(&symbol) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(r),
            }
        }
        None
    };
    for i in 0..declared {
        let r = row(i);
        if i > 0 && row(i - 1).symbol >= r.symbol {
            return Err(FormError::DeclaredUnsorted { row: i as u32 });
        }
        if !r.kind.fits(r.item, r.len) {
            return Err(FormError::DeclaredShape { symbol: r.symbol, kind: r.kind });
        }
        if !has_target(Target { space: r.kind.space(), symbol: r.symbol }) {
            return Err(FormError::DeclaredUnused { symbol: r.symbol, kind: r.kind });
        }
    }
    for t in 0..targets {
        let tg = target(t);
        if tg.space == Space::Outside {
            continue;
        }
        match row_of(tg.symbol) {
            None => return Err(FormError::DeclaredMissing { symbol: tg.symbol, space: tg.space }),
            Some(r) if r.kind.space() != tg.space => {
                return Err(FormError::DeclaredSpace { symbol: tg.symbol, space: tg.space, kind: r.kind });
            }
            Some(_) => {}
        }
    }
    Ok(())
}
