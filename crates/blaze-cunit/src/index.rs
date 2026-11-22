//! The symbol index: which unit defines each C name a program can reach (c.md part 3: "an index
//! from symbol to unit is a cheap record made from the units' declarations").
//!
//! It is made from each unit's DEFINITIONS ([`UnitDefs`]): the global names the unit defines and
//! how each binds ([`Strength`]). Those come from the front end alone (the `check` pass of the
//! unit: parsed and type-checked, no code made), never from its object, so the index is had
//! without compiling a unit; what the front end must report is in lib.rs ("What the driver and
//! blaze-c give").
//!
//! **The rule, one name at a time** (deterministic: a function of the set of units alone, never of
//! the order they are given in, of a map, or of the machine):
//!
//! - two STRONG definitions are a refusal naming both units ([`IndexError::Twice`]); with three
//!   or more, the two least unit keys are named;
//! - one strong definition wins over every weak and COMDAT one;
//! - with no strong one, the definition of the LEAST UNIT KEY is chosen among the weak and COMDAT
//!   ones. A linker takes the first in link order; blaze has no link order, and a key order is the
//!   one order every builder of the same units agrees on (rule 25). For a COMDAT (C++ inline
//!   functions, template instances) every definition is the same by the ODR, so any choice is
//!   right; for two weak C definitions that differ, this is the choice, stated;
//! - definitions of one name that disagree on what it is (a function and a data object) are a
//!   refusal naming both units ([`IndexError::Kinds`]);
//! - a unit listing one name twice is a refusal naming it ([`IndexError::TwiceInUnit`]).
//!
//! **The record.** `MAGIC`, then the row count (u32), then each row's offset (u32, from the
//! record's first byte), then the rows sorted by name, each: the name's length (u32), the name, the
//! unit key (32), the strength's byte, the kind's byte. Read in place ([`IndexView`]), checked once
//! ([`IndexView::read`]), looked up by binary search.

use core::fmt;

use blaze_own::planted::{Planted, Plants};
use blaze_own::{AVec, Alloc, Vec};

use crate::key::UnitKey;
use crate::split::{Shown, Strength};

/// The first eight bytes of the symbol index record.
pub const INDEX_MAGIC: [u8; 8] = *b"bzcsym01";

/// The bugs a control can plant in the index (D165).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexPlant {
    /// The index as it is.
    None,
    /// Among weak definitions, the first GIVEN is chosen (a linker's link order), not the least
    /// key: the index then depends on the order units are listed in.
    FirstGiven,
}

impl Plants for IndexPlant {}

/// What a C name is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefKind {
    Func,
    Data,
    ThreadLocal,
}

impl DefKind {
    fn byte(self) -> u8 {
        match self {
            DefKind::Func => 0,
            DefKind::Data => 1,
            DefKind::ThreadLocal => 2,
        }
    }

    fn of(byte: u8) -> Option<DefKind> {
        [DefKind::Func, DefKind::Data, DefKind::ThreadLocal].get(usize::from(byte)).copied()
    }
}

fn strength_byte(s: Strength) -> u8 {
    match s {
        Strength::Local => 0,
        Strength::Strong => 1,
        Strength::Weak => 2,
        Strength::Comdat => 3,
    }
}

fn strength_of(byte: u8) -> Option<Strength> {
    [Strength::Local, Strength::Strong, Strength::Weak, Strength::Comdat].get(usize::from(byte)).copied()
}

/// One global definition of a unit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Def<'a> {
    /// The C name (no Mach-O `_`; a C++ name mangled, as clang mangles it).
    pub name: &'a [u8],
    /// Never [`Strength::Local`]: a local name is no other unit's to reach.
    pub strength: Strength,
    pub kind: DefKind,
}

/// One unit's global definitions.
#[derive(Clone, Copy, Debug)]
pub struct UnitDefs<'a> {
    pub unit: UnitKey,
    pub defs: &'a [Def<'a>],
}

/// Why there is no index: each names the C name and the units.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexError<'a> {
    /// Two strong definitions of `name` (the two least unit keys among them).
    Twice { name: Shown<'a>, first: UnitKey, second: UnitKey },
    /// `name` is a function in one unit and a data object in another.
    Kinds { name: Shown<'a>, first: (UnitKey, DefKind), second: (UnitKey, DefKind) },
    /// `unit` lists `name` twice.
    TwiceInUnit { name: Shown<'a>, unit: UnitKey },
    /// `unit` lists `name` as local.
    Local { name: Shown<'a>, unit: UnitKey },
}

impl fmt::Display for IndexError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = "(blaze-cunit index.rs; c.md part 3)";
        match *self {
            IndexError::Twice { name, first, second } => write!(f, "C name {name} is defined by unit {first} and by unit {second}, neither weak: one definition a program {at}"),
            IndexError::Kinds { name, first, second } => write!(f, "C name {name} is a {:?} in unit {} and a {:?} in unit {} {at}", first.1, first.0, second.1, second.0),
            IndexError::TwiceInUnit { name, unit } => write!(f, "unit {unit} lists C name {name} among its definitions twice {at}"),
            IndexError::Local { name, unit } => write!(f, "unit {unit} lists the local name {name} among its global definitions {at}"),
        }
    }
}

impl core::error::Error for IndexError<'_> {}

/// One row: a name and the definition chosen for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row<'a> {
    pub name: &'a [u8],
    pub unit: UnitKey,
    pub strength: Strength,
    pub kind: DefKind,
}

/// Builds the index's record of `units` (module docs) into `out`. `plant` is
/// [`blaze_own::planted::NoPlant`] in shipped code.
pub fn build<'a>(units: &[UnitDefs<'a>], out: &mut AVec<'_, u8>, alloc: Alloc<'_>, plant: impl Planted<IndexPlant>) -> Result<(), IndexError<'a>> {
    // Every definition, with the place it was given at (for the plant alone).
    let mut all: AVec<'_, (Row<'a>, (u32, u32))> = Vec::new_in(alloc);
    for (given, u) in units.iter().enumerate() {
        for (j, d) in u.defs.iter().enumerate() {
            if d.strength == Strength::Local {
                return Err(IndexError::Local { name: Shown(d.name), unit: u.unit });
            }
            all.push((Row { name: d.name, unit: u.unit, strength: d.strength, kind: d.kind }, (given as u32, j as u32)));
        }
    }
    if plant.has(IndexPlant::FirstGiven) {
        all.sort_unstable_by(|a, b| (a.0.name, a.1).cmp(&(b.0.name, b.1)));
    } else {
        all.sort_unstable_by(|a, b| (a.0.name, a.0.unit).cmp(&(b.0.name, b.0.unit)));
    }
    let mut chosen: AVec<'_, Row<'a>> = Vec::new_in(alloc);
    let mut i = 0;
    while i < all.len() {
        let name = all[i].0.name;
        let end = i + all[i..].iter().take_while(|d| d.0.name == name).count();
        let group = &all[i..end];
        let mut strong: Option<&Row<'a>> = None;
        for (k, (d, _)) in group.iter().enumerate() {
            if k > 0 && group[k - 1].0.unit == d.unit {
                return Err(IndexError::TwiceInUnit { name: Shown(name), unit: d.unit });
            }
            let first = &group[0].0;
            if d.kind != first.kind {
                return Err(IndexError::Kinds { name: Shown(name), first: (first.unit, first.kind), second: (d.unit, d.kind) });
            }
            if d.strength == Strength::Strong {
                if let Some(s) = strong {
                    return Err(IndexError::Twice { name: Shown(name), first: s.unit, second: d.unit });
                }
                strong = Some(d);
            }
        }
        chosen.push(*strong.unwrap_or(&group[0].0));
        i = end;
    }
    encode(&chosen, out);
    Ok(())
}

/// The rows, sorted by name, as the record's bytes.
fn encode(rows: &[Row<'_>], out: &mut AVec<'_, u8>) {
    let start = out.len();
    out.extend_from_slice(&INDEX_MAGIC);
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    let table = out.len();
    out.resize(table + 4 * rows.len(), 0);
    for (n, row) in rows.iter().enumerate() {
        let at = (out.len() - start) as u32;
        out[table + 4 * n..table + 4 * n + 4].copy_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&(row.name.len() as u32).to_le_bytes());
        out.extend_from_slice(row.name);
        out.extend_from_slice(&row.unit.0);
        out.push(strength_byte(row.strength));
        out.push(row.kind.byte());
    }
}

/// Why bytes are not an index record: the first broken rule and where.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexReadError {
    Magic,
    /// A field at `at` runs past the end.
    Length { at: usize },
    /// Row `row`'s strength or kind byte names nothing.
    Byte { row: u32 },
    /// Row `row`'s name is not after row `row - 1`'s.
    Order { row: u32 },
}

impl fmt::Display for IndexReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let form = "the symbol index record (blaze-cunit index.rs)";
        match *self {
            IndexReadError::Magic => write!(f, "the bytes are not {form}: no `bzcsym01` head"),
            IndexReadError::Length { at } => write!(f, "{form}'s field at {at} runs past its end"),
            IndexReadError::Byte { row } => write!(f, "{form}'s row {row} has a strength or kind byte that names nothing"),
            IndexReadError::Order { row } => write!(f, "{form}'s row {row} is not after the row before it by name"),
        }
    }
}

impl core::error::Error for IndexReadError {}

/// An index record read in place.
#[derive(Clone, Copy)]
pub struct IndexView<'b> {
    bytes: &'b [u8],
    rows: u32,
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, IndexReadError> {
    let s = bytes.get(at..at.checked_add(4).ok_or(IndexReadError::Length { at })?).ok_or(IndexReadError::Length { at })?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

impl<'b> IndexView<'b> {
    /// Checks every rule once (rule 74): every row inside, its bytes known, names in order.
    pub fn read(bytes: &'b [u8]) -> Result<IndexView<'b>, IndexReadError> {
        if bytes.get(..8) != Some(&INDEX_MAGIC[..]) {
            return Err(IndexReadError::Magic);
        }
        let rows = u32_at(bytes, 8)?;
        let view = IndexView { bytes, rows };
        let mut previous: Option<&[u8]> = None;
        for n in 0..rows {
            let row = view.checked(n)?;
            if previous.is_some_and(|p| p >= row.name) {
                return Err(IndexReadError::Order { row: n });
            }
            previous = Some(row.name);
        }
        Ok(view)
    }

    fn checked(&self, n: u32) -> Result<Row<'b>, IndexReadError> {
        let at = u32_at(self.bytes, 12 + 4 * n as usize)? as usize;
        let len = u32_at(self.bytes, at)? as usize;
        let name_at = at + 4;
        let tail_at = name_at.checked_add(len).ok_or(IndexReadError::Length { at })?;
        let name = self.bytes.get(name_at..tail_at).ok_or(IndexReadError::Length { at: name_at })?;
        let tail = self.bytes.get(tail_at..tail_at + 34).ok_or(IndexReadError::Length { at: tail_at })?;
        let mut unit = [0u8; 32];
        unit.copy_from_slice(&tail[..32]);
        let strength = strength_of(tail[32]).ok_or(IndexReadError::Byte { row: n })?;
        let kind = DefKind::of(tail[33]).ok_or(IndexReadError::Byte { row: n })?;
        Ok(Row { name, unit: UnitKey(unit), strength, kind })
    }

    pub fn len(&self) -> usize {
        self.rows as usize
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Row `n`; every row was checked by [`IndexView::read`].
    pub fn row(&self, n: usize) -> Row<'b> {
        match self.checked(n as u32) {
            Ok(row) => row,
            Err(_) => unreachable!("an index row checked by IndexView::read no longer reads"),
        }
    }

    /// The definition chosen for `name`, if any unit defines it.
    pub fn find(&self, name: &[u8]) -> Option<Row<'b>> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let row = self.row(mid);
            match row.name.cmp(name) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(row),
            }
        }
        None
    }
}
