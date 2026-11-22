//! A value while a query runs ([`Datum`]), and the [`Store`] that holds its text and lists.
//!
//! A datum is 16 bytes and `Copy`: text and lists are ids into the query's store, so equal
//! text is an equal id (the store interns), and a column of datums is a plain array. An
//! integer is exact: an `i64`, or a `u64` above `i64::MAX` (the profile's keys are full
//! `u64`s), one representation per number, so equality is the numbers'. Arithmetic is done in
//! `i128` and refused past the 64-bit range.

use core::alloc::Allocator;
use core::cmp::Ordering;
use core::hash::{BuildHasher, Hasher};

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::Vec;

/// Interned text: an id into a [`Store`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sym(u32);

/// A list: an id into a [`Store`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListId(u32);

/// One value.
#[derive(Clone, Copy, Debug)]
pub enum Datum {
    Null,
    Bool(bool),
    Int(i64),
    /// Above `i64::MAX`, always: below it the number is an [`Datum::Int`].
    UInt(u64),
    Float(f64),
    Text(Sym),
    List(ListId),
}

/// A datum's kind, for messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Null,
    Bool,
    Int,
    Float,
    Text,
    List,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Null => "null",
            Kind::Bool => "a boolean",
            Kind::Int => "an integer",
            Kind::Float => "a double",
            Kind::Text => "text",
            Kind::List => "a list",
        }
    }
}

/// Two datums of kinds no operator relates (text and an integer).
#[derive(Clone, Copy, Debug)]
pub struct Mismatch {
    pub left: Kind,
    pub right: Kind,
}

impl Datum {
    /// The integer `n` if it is within the 64-bit range (`i64::MIN..=u64::MAX`).
    pub fn int(n: i128) -> Option<Datum> {
        if let Ok(small) = i64::try_from(n) {
            return Some(Datum::Int(small));
        }
        u64::try_from(n).ok().map(Datum::UInt)
    }

    pub fn from_u64(n: u64) -> Datum {
        match i64::try_from(n) {
            Ok(small) => Datum::Int(small),
            Err(_) => Datum::UInt(n),
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Datum::Null => Kind::Null,
            Datum::Bool(_) => Kind::Bool,
            Datum::Int(_) | Datum::UInt(_) => Kind::Int,
            Datum::Float(_) => Kind::Float,
            Datum::Text(_) => Kind::Text,
            Datum::List(_) => Kind::List,
        }
    }

    pub fn is_null(self) -> bool {
        matches!(self, Datum::Null)
    }

    /// An integer's value.
    pub fn as_i128(self) -> Option<i128> {
        match self {
            Datum::Int(n) => Some(i128::from(n)),
            Datum::UInt(n) => Some(i128::from(n)),
            _ => None,
        }
    }

    /// A number's value as a double (an integer the nearest double, as SQL casts it).
    #[expect(blaze::as_cast, reason = "an integer to its nearest double, as SQL casts it")]
    pub fn as_f64(self) -> Option<f64> {
        match self {
            Datum::Int(n) => Some(n as f64),
            Datum::UInt(n) => Some(n as f64),
            Datum::Float(f) => Some(f),
            _ => None,
        }
    }
}

/// Compares an integer with a double exactly: by the integer where the double is one in
/// range, else by doubles. NaN is above every number.
#[expect(blaze::as_cast, reason = "a double to an integer only after checking it is one in range")]
fn int_float(i: i128, f: f64) -> Ordering {
    if f.is_nan() {
        return Ordering::Less;
    }
    // 2^127 is exactly representable; every double below it in magnitude fits i128.
    const LIMIT: f64 = 1.7014118346046923e38;
    if f.fract() == 0.0 && f.abs() < LIMIT {
        return i.cmp(&(f as i128));
    }
    let as_float = i as f64;
    as_float.partial_cmp(&f).unwrap_or(Ordering::Equal)
}

/// Doubles in a total order: NaN above every number and equal to itself, `-0` equal to `0`.
fn floats(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// The text and lists of one query, and how much text it may hold.
pub struct Store<A: Allocator + Copy> {
    bytes: Vec<u8, A>,
    /// Each symbol's bytes: start and length in `bytes`.
    spans: Vec<(u32, u32), A>,
    /// The first symbol of each hash; the rest of a chain through `next`.
    index: Map<u64, u32, FixedState, A>,
    next: Vec<u32, A>,
    lists: Vec<(u32, u32), A>,
    items: Vec<Datum, A>,
    most_bytes: usize,
}

/// No next symbol in a hash chain.
const END: u32 = u32::MAX;

/// The store would pass its bound: what, and the bound.
#[derive(Clone, Copy, Debug)]
pub struct Full {
    pub what: &'static str,
    pub most: u64,
}

impl<A: Allocator + Copy> Store<A> {
    pub fn new_in(most_bytes: usize, alloc: A) -> Store<A> {
        Store {
            bytes: Vec::new_in(alloc),
            spans: Vec::new_in(alloc),
            index: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            next: Vec::new_in(alloc),
            lists: Vec::new_in(alloc),
            items: Vec::new_in(alloc),
            most_bytes,
        }
    }

    fn full(&self) -> Full {
        Full { what: "bytes of distinct text", most: u64::try_from(self.most_bytes).unwrap_or(u64::MAX) }
    }

    /// The symbol of `text`, interning it if new.
    pub fn intern(&mut self, text: &str) -> Result<Sym, Full> {
        let hash = FixedState.hash_one(text.as_bytes());
        let mut at = self.index.get(&hash).copied().unwrap_or(END);
        // A chain holds only symbols of one hash, each taken once.
        while at != END {
            if self.text(Sym(at)) == text {
                return Ok(Sym(at));
            }
            at = self.next.get(at as usize).copied().unwrap_or(END);
        }
        if self.bytes.len() + text.len() > self.most_bytes || self.spans.len() >= END as usize {
            return Err(self.full());
        }
        let full = self.full();
        let start = u32::try_from(self.bytes.len()).map_err(|_| full)?;
        let len = u32::try_from(text.len()).map_err(|_| full)?;
        let sym = u32::try_from(self.spans.len()).map_err(|_| full)?;
        self.bytes.extend_from_slice(text.as_bytes());
        self.spans.push((start, len));
        let first = self.index.insert(hash, sym).unwrap_or(END);
        self.next.push(first);
        Ok(Sym(sym))
    }

    /// The text of `sym`.
    pub fn text(&self, sym: Sym) -> &str {
        let Some(&(start, len)) = self.spans.get(sym.0 as usize) else {
            return "";
        };
        let range = start as usize..start as usize + len as usize;
        // Every span was cut from a `&str` at its ends.
        self.bytes.get(range).and_then(|bytes| core::str::from_utf8(bytes).ok()).unwrap_or("")
    }

    /// A new list of `items`.
    pub fn list(&mut self, items: &[Datum]) -> Result<ListId, Full> {
        let full = Full { what: "list elements", most: u64::from(u32::MAX) };
        let start = u32::try_from(self.items.len()).map_err(|_| full)?;
        let len = u32::try_from(items.len()).map_err(|_| full)?;
        let id = u32::try_from(self.lists.len()).map_err(|_| full)?;
        self.items.extend_from_slice(items);
        self.lists.push((start, len));
        Ok(ListId(id))
    }

    pub fn items(&self, list: ListId) -> &[Datum] {
        let Some(&(start, len)) = self.lists.get(list.0 as usize) else {
            return &[];
        };
        self.items.get(start as usize..start as usize + len as usize).unwrap_or(&[])
    }

    /// SQL's comparison of two non-null datums: `None` where either is null.
    pub fn compare(&self, a: Datum, b: Datum) -> Result<Option<Ordering>, Mismatch> {
        if a.is_null() || b.is_null() {
            return Ok(None);
        }
        self.order(a, b).map(Some)
    }

    /// The total order a sort uses for two datums of comparable kinds; null is the caller's.
    /// Numbers by value across kinds, text by bytes, `false` before `true`, lists element by
    /// element and then by length.
    pub fn order(&self, a: Datum, b: Datum) -> Result<Ordering, Mismatch> {
        let mismatch = Mismatch { left: a.kind(), right: b.kind() };
        Ok(match (a, b) {
            (Datum::Null, Datum::Null) => Ordering::Equal,
            (Datum::Null, _) => Ordering::Greater,
            (_, Datum::Null) => Ordering::Less,
            (Datum::Bool(x), Datum::Bool(y)) => x.cmp(&y),
            (Datum::Text(x), Datum::Text(y)) => {
                if x == y {
                    Ordering::Equal
                } else {
                    self.text(x).as_bytes().cmp(self.text(y).as_bytes())
                }
            }
            (Datum::Float(x), Datum::Float(y)) => floats(x, y),
            (Datum::Float(x), y @ (Datum::Int(_) | Datum::UInt(_))) => {
                int_float(y.as_i128().unwrap_or(0), x).reverse()
            }
            (x @ (Datum::Int(_) | Datum::UInt(_)), Datum::Float(y)) => {
                int_float(x.as_i128().unwrap_or(0), y)
            }
            (x @ (Datum::Int(_) | Datum::UInt(_)), y @ (Datum::Int(_) | Datum::UInt(_))) => {
                x.as_i128().cmp(&y.as_i128())
            }
            (Datum::List(x), Datum::List(y)) => {
                let (xs, ys) = (self.items(x), self.items(y));
                for (p, q) in xs.iter().zip(ys) {
                    let it = self.order(*p, *q)?;
                    if it != Ordering::Equal {
                        return Ok(it);
                    }
                }
                xs.len().cmp(&ys.len())
            }
            _ => return Err(mismatch),
        })
    }

    /// Whether two datums are one group's key, a `distinct` duplicate or a `union` duplicate:
    /// nulls are the same as each other, numbers by value, NaN the same as NaN.
    pub fn same(&self, a: Datum, b: Datum) -> bool {
        match (a, b) {
            (Datum::Null, Datum::Null) => true,
            (Datum::Null, _) | (_, Datum::Null) => false,
            (Datum::Text(x), Datum::Text(y)) => x == y,
            _ => self.order(a, b).is_ok_and(|it| it == Ordering::Equal),
        }
    }

    /// Feeds `d` to `hasher` so that [`Store::same`] datums hash alike: an integral double as
    /// the integer it equals.
    #[expect(blaze::as_cast, reason = "a double to an integer only after checking it is one in range")]
    pub fn hash(&self, d: Datum, hasher: &mut impl Hasher) {
        match d {
            Datum::Null => hasher.write_u8(0),
            Datum::Bool(b) => {
                hasher.write_u8(1);
                hasher.write_u8(u8::from(b));
            }
            Datum::Int(_) | Datum::UInt(_) => {
                hasher.write_u8(2);
                hasher.write_i128(d.as_i128().unwrap_or(0));
            }
            Datum::Float(f) => {
                const LIMIT: f64 = 1.7014118346046923e38;
                if f.abs() < LIMIT && (f as i128) as f64 == f {
                    hasher.write_u8(2);
                    hasher.write_i128(f as i128);
                } else if f.is_nan() {
                    hasher.write_u8(3);
                } else {
                    hasher.write_u8(3);
                    hasher.write_u64(f.to_bits());
                }
            }
            Datum::Text(sym) => {
                hasher.write_u8(4);
                hasher.write_u32(sym.0);
            }
            Datum::List(list) => {
                hasher.write_u8(5);
                let items = self.items(list);
                hasher.write_usize(items.len());
                for item in items {
                    self.hash(*item, hasher);
                }
            }
        }
    }

    /// One hash of a row's key datums.
    pub fn hash_key(&self, key: &[Datum]) -> u64 {
        let mut hasher = FixedState.build_hasher();
        for d in key {
            self.hash(*d, &mut hasher);
        }
        hasher.finish()
    }
}
