//! [`Value`], [`Int`] and [`Kind`].

#![blaze::sans_io]

use core::{alloc::Allocator, cmp::Ordering, fmt};

use blaze_own::{String, Vec};

use crate::{Build, Map, Order, Visit, Walk};

/// An integer as written: any `u64` or any `i64`, one representation per number (a
/// non-negative one is always held as `u64`), so equality is the numbers' equality.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Int(Repr);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Repr {
    NonNegative(u64),
    /// Below zero, always.
    Negative(i64),
}

impl Int {
    pub const fn from_u64(n: u64) -> Int {
        Int(Repr::NonNegative(n))
    }

    pub fn from_i64(n: i64) -> Int {
        match u64::try_from(n) {
            Ok(n) => Int(Repr::NonNegative(n)),
            Err(_) => Int(Repr::Negative(n)),
        }
    }

    pub fn as_u64(self) -> Option<u64> {
        match self.0 {
            Repr::NonNegative(n) => Some(n),
            Repr::Negative(_) => None,
        }
    }

    pub fn as_i64(self) -> Option<i64> {
        match self.0 {
            Repr::NonNegative(n) => i64::try_from(n).ok(),
            Repr::Negative(n) => Some(n),
        }
    }

    /// Every `Int` fits.
    pub fn as_i128(self) -> i128 {
        match self.0 {
            Repr::NonNegative(n) => i128::from(n),
            Repr::Negative(n) => i128::from(n),
        }
    }

    pub fn is_negative(self) -> bool {
        matches!(self.0, Repr::Negative(_))
    }

    /// The number as a float when the float is exactly it (`|n| <= 2^53`, or a power of two's
    /// multiple that lands exactly), else `None`.
    pub fn as_f64_exact(self) -> Option<f64> {
        #[expect(blaze::as_cast, reason = "rounded on purpose, then checked to be exact")]
        let f = self.as_i128() as f64;
        #[expect(blaze::as_cast, reason = "the float is finite and within i128, checked back")]
        let back = f as i128;
        (back == self.as_i128()).then_some(f)
    }
}

impl PartialOrd for Int {
    fn partial_cmp(&self, other: &Int) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Int {
    fn cmp(&self, other: &Int) -> Ordering {
        self.as_i128().cmp(&other.as_i128())
    }
}

impl fmt::Display for Int {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Repr::NonNegative(n) => write!(f, "{n}"),
            Repr::Negative(n) => write!(f, "{n}"),
        }
    }
}

impl From<u64> for Int {
    fn from(n: u64) -> Int {
        Int::from_u64(n)
    }
}

impl From<i64> for Int {
    fn from(n: i64) -> Int {
        Int::from_i64(n)
    }
}

/// What a value is, for errors and dispatch.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Null,
    Bool,
    Int,
    Float,
    Text,
    Bytes,
    List,
    Map,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Null => "null",
            Kind::Bool => "bool",
            Kind::Int => "integer",
            Kind::Float => "float",
            Kind::Text => "text",
            Kind::Bytes => "bytes",
            Kind::List => "list",
            Kind::Map => "map",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A format-neutral value; see the crate's documentation.
pub enum Value<A: Allocator> {
    Null,
    Bool(bool),
    Int(Int),
    Float(f64),
    Text(String<A>),
    Bytes(Vec<u8, A>),
    List(Vec<Value<A>, A>),
    Map(Map<A>),
}

/// A drop without recursion, so a value read from untrusted bytes at any depth a reader
/// accepts cannot overflow the stack as it goes: each container's descendants are moved into
/// its own storage (a list's `Vec`, a map's entries) and taken out one at a time, so every
/// value dropped meanwhile is a leaf or an emptied container. No memory is asked for but the
/// growth of those, bounded by the values held.
impl<A: Allocator> Drop for Value<A> {
    fn drop(&mut self) {
        match self {
            Value::List(list) => drain_list(list),
            Value::Map(map) => {
                while let Some((_, mut child)) = map.pop_for_drop() {
                    match &mut child {
                        Value::List(list) => drain_list(list),
                        Value::Map(inner) => {
                            while let Some(entry) = inner.pop_for_drop() {
                                map.push_for_drop(entry);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn drain_list<A: Allocator>(list: &mut Vec<Value<A>, A>) {
    while let Some(mut child) = list.pop() {
        match &mut child {
            Value::List(inner) => {
                while let Some(value) = inner.pop() {
                    list.push(value);
                }
            }
            Value::Map(inner) => {
                while let Some((_, value)) = inner.pop_for_drop() {
                    list.push(value);
                }
            }
            _ => {}
        }
    }
}

/// Owners taken out. `Value` has a `Drop`, so `match v { Value::Map(m) => m, .. }` by value
/// does not compile; these take the owner and leave an empty one in the same allocator.
impl<A: Allocator + Clone> Value<A> {
    pub fn into_text(mut self) -> Option<String<A>> {
        match &mut self {
            Value::Text(text) => {
                let empty = String::new_in(text.allocator().clone());
                Some(core::mem::replace(text, empty))
            }
            _ => None,
        }
    }

    pub fn into_bytes(mut self) -> Option<Vec<u8, A>> {
        match &mut self {
            Value::Bytes(bytes) => Some(bytes.split_off(0)),
            _ => None,
        }
    }

    pub fn into_list(mut self) -> Option<Vec<Value<A>, A>> {
        match &mut self {
            Value::List(list) => Some(list.split_off(0)),
            _ => None,
        }
    }

    pub fn into_map(mut self) -> Option<Map<A>> {
        match &mut self {
            Value::Map(map) => Some(map.take()),
            _ => None,
        }
    }
}

impl<A: Allocator> Value<A> {
    pub fn kind(&self) -> Kind {
        match self {
            Value::Null => Kind::Null,
            Value::Bool(_) => Kind::Bool,
            Value::Int(_) => Kind::Int,
            Value::Float(_) => Kind::Float,
            Value::Text(_) => Kind::Text,
            Value::Bytes(_) => Kind::Bytes,
            Value::List(_) => Kind::List,
            Value::Map(_) => Kind::Map,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<Int> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_int().and_then(Int::as_u64)
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_int().and_then(Int::as_i64)
    }

    /// A float, or an integer the float holds exactly.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(n) => n.as_f64_exact(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text.as_str()),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(bytes) => Some(bytes.as_slice()),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value<A>]> {
        match self {
            Value::List(list) => Some(list.as_slice()),
            _ => None,
        }
    }

    pub fn as_list_mut(&mut self) -> Option<&mut Vec<Value<A>, A>> {
        match self {
            Value::List(list) => Some(list),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map<A>> {
        match self {
            Value::Map(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_map_mut(&mut self) -> Option<&mut Map<A>> {
        match self {
            Value::Map(map) => Some(map),
            _ => None,
        }
    }

    /// The value under `key` of a map; `None` for a missing key or a value that is no map.
    pub fn get(&self, key: &str) -> Option<&Value<A>> {
        self.as_map().and_then(|map| map.get(key))
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value<A>> {
        self.as_map_mut().and_then(|map| map.get_mut(key))
    }

    /// The element at `index` of a list.
    pub fn at(&self, index: usize) -> Option<&Value<A>> {
        self.as_list().and_then(|list| list.get(index))
    }

    /// A deep copy into `alloc`, walked without recursion (its stack in `alloc` too).
    pub fn copy_in<B: Allocator + Clone>(&self, alloc: B) -> Value<B> {
        let mut build = Build::new_in(alloc.clone(), alloc.clone());
        for visit in Walk::new_in(self, Order::Insertion, alloc.clone()) {
            match visit {
                Visit::Leaf(leaf) => build.value(leaf.copy_leaf_in(alloc.clone())),
                Visit::StartList(list) => build.start_list(list.len()),
                Visit::StartMap(map) => build.start_map(map.len()),
                Visit::Item(_) => {}
                // The source's keys are unique, so none replaces.
                Visit::Key(_, key) => build.key_last_wins(String::from_str_in(key, alloc.clone())),
                Visit::EndList(_) | Visit::EndMap(_) => build.end(),
            }
        }
        match build.finish() {
            Some(value) => value,
            None => unreachable!("a walk closes everything it opens"),
        }
    }

    /// A copy of a value that is no list or map.
    fn copy_leaf_in<B: Allocator + Clone>(&self, alloc: B) -> Value<B> {
        match self {
            Value::Null => Value::Null,
            Value::Bool(b) => Value::Bool(*b),
            Value::Int(n) => Value::Int(*n),
            Value::Float(f) => Value::Float(*f),
            Value::Text(text) => Value::Text(String::from_str_in(text.as_str(), alloc)),
            Value::Bytes(bytes) => {
                let mut out = Vec::with_capacity_in(bytes.len(), alloc);
                out.extend_from_slice(bytes.as_slice());
                Value::Bytes(out)
            }
            Value::List(_) | Value::Map(_) => unreachable!("a walk visits containers as starts"),
        }
    }
}

impl<A: Allocator + Clone> Clone for Value<A> {
    fn clone(&self) -> Value<A> {
        match self {
            Value::Null => Value::Null,
            Value::Bool(b) => Value::Bool(*b),
            Value::Int(n) => Value::Int(*n),
            Value::Float(f) => Value::Float(*f),
            Value::Text(text) => Value::Text(text.clone()),
            Value::Bytes(bytes) => Value::Bytes(bytes.clone()),
            Value::List(list) => Value::List(list.clone()),
            Value::Map(map) => Value::Map(map.clone()),
        }
    }
}

/// Structural; floats by bits; maps by content, whatever their insertion orders.
impl<A: Allocator, B: Allocator> PartialEq<Value<B>> for Value<A> {
    fn eq(&self, other: &Value<B>) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Text(a), Value::Text(b)) => a.as_str() == b.as_str(),
            (Value::Bytes(a), Value::Bytes(b)) => a.as_slice() == b.as_slice(),
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
            }
            (Value::Map(a), Value::Map(b)) => a == b,
            _ => false,
        }
    }
}

impl<A: Allocator> Eq for Value<A> {}

impl<A: Allocator> PartialEq<str> for Value<A> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == Some(other)
    }
}

impl<A: Allocator> PartialEq<&str> for Value<A> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == Some(*other)
    }
}

impl<A: Allocator> PartialEq<bool> for Value<A> {
    fn eq(&self, other: &bool) -> bool {
        self.as_bool() == Some(*other)
    }
}

impl<A: Allocator> PartialEq<u64> for Value<A> {
    fn eq(&self, other: &u64) -> bool {
        self.as_int() == Some(Int::from_u64(*other))
    }
}

impl<A: Allocator> PartialEq<i64> for Value<A> {
    fn eq(&self, other: &i64) -> bool {
        self.as_int() == Some(Int::from_i64(*other))
    }
}

impl<A: Allocator> PartialEq<i32> for Value<A> {
    fn eq(&self, other: &i32) -> bool {
        self.as_int() == Some(Int::from_i64(i64::from(*other)))
    }
}

/// CBOR's diagnostic notation (RFC 8949, section 8): `{"a": [1, 2.5, h'00ff', null]}`, maps in
/// insertion order. For tests and messages; a format's writer is that format's crate.
impl<A: Allocator> fmt::Debug for Value<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) if x.is_nan() => f.write_str("NaN"),
            Value::Float(x) if x.is_infinite() => {
                f.write_str(if x.is_sign_negative() { "-Infinity" } else { "Infinity" })
            }
            Value::Float(x) => write!(f, "{x:?}"),
            Value::Text(text) => write!(f, "{:?}", text.as_str()),
            Value::Bytes(bytes) => {
                f.write_str("h'")?;
                for b in bytes.iter() {
                    write!(f, "{b:02x}")?;
                }
                f.write_str("'")
            }
            Value::List(list) => {
                f.write_str("[")?;
                for (i, item) in list.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item:?}")?;
                }
                f.write_str("]")
            }
            Value::Map(map) => write!(f, "{map:?}"),
        }
    }
}
