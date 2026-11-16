//! Columns of rows in memory, in place of Arrow's arrays and record batches
//! (docs/v2/plan/third-party.md, "The profile's file"): what blaze-parquet writes and reads,
//! and blaze-arrow-ipc sends to the explorer and the live feed's clients.
//!
//! A [`Column`] is one plain buffer of values ([`Values`]: numbers, text as offsets into one
//! string, or dictionary keys over such text), a validity when some row is null, and, for a
//! list column, the offsets of each row's elements in the values (whose elements are never
//! null). A [`Batch`] is named columns ([`Field`]) of one length. Nothing is shared or counted:
//! a batch owns its buffers, in the allocator its maker names, and is read through typed views
//! ([`Prim`], [`Strs`], [`Lists`]) that say `None` where the column is of another type.
//!
//! What a row holds where it is null is unspecified (the constructors write zero and the empty
//! string); only its validity says it is null.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes, portable_simd)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

use core::{alloc::Allocator, fmt, ops::Range};

use blaze_int::Int;
use blaze_own::{String, Vec};

/// Lightweight encodings read in place (docs/v2/plan/design/columns.md).
pub mod enc;
#[cfg(test)]
mod tests;

/// The type of one value: a column's, or a list column's elements'.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    Bool,
    I32,
    I64,
    U16,
    U32,
    U64,
    F64,
    /// UTF-8 text.
    Text,
    /// UTF-8 text from a small set, as `u32` keys into its distinct values.
    Dict,
}

impl Scalar {
    /// Whether it is text, plain or keyed.
    pub const fn is_text(self) -> bool {
        matches!(self, Scalar::Text | Scalar::Dict)
    }
}

/// A column's type: one value a row, or a list of values a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    One(Scalar),
    /// A list a row, its elements of the scalar type and never null.
    List(Scalar),
}

impl Type {
    /// The type of a value: the column's, or its elements'.
    pub const fn scalar(self) -> Scalar {
        match self {
            Type::One(it) | Type::List(it) => it,
        }
    }
}

/// A column's name: a constant of its schema, or read from a file.
pub enum Name<A: Allocator> {
    Static(&'static str),
    Owned(String<A>),
}

impl<A: Allocator> Name<A> {
    pub fn as_str(&self) -> &str {
        match self {
            Name::Static(it) => it,
            Name::Owned(it) => it.as_str(),
        }
    }
}

impl<A: Allocator + Clone> Clone for Name<A> {
    fn clone(&self) -> Name<A> {
        match self {
            Name::Static(it) => Name::Static(it),
            // clone: a name read from a file, as its batch is cloned
            Name::Owned(it) => Name::Owned(it.clone()),
        }
    }
}

impl<A: Allocator> PartialEq for Name<A> {
    fn eq(&self, other: &Name<A>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<A: Allocator> fmt::Debug for Name<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<A: Allocator> fmt::Display for Name<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A column of a batch: its name, its type, and whether a row of it may be null.
pub struct Field<A: Allocator> {
    pub name: Name<A>,
    pub ty: Type,
    pub nullable: bool,
}

impl<A: Allocator> Field<A> {
    /// The field of a schema's constant name.
    pub const fn new(name: &'static str, ty: Type, nullable: bool) -> Field<A> {
        Field { name: Name::Static(name), ty, nullable }
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }
}

impl<A: Allocator + Clone> Clone for Field<A> {
    fn clone(&self) -> Field<A> {
        Field { name: self.name.clone(), ty: self.ty, nullable: self.nullable }
    }
}

impl<A: Allocator> PartialEq for Field<A> {
    fn eq(&self, other: &Field<A>) -> bool {
        self.name == other.name && self.ty == other.ty && self.nullable == other.nullable
    }
}

impl<A: Allocator> fmt::Debug for Field<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Field")
            .field("name", &self.name)
            .field("ty", &self.ty)
            .field("nullable", &self.nullable)
            .finish()
    }
}

/// Why a column or a batch could not be made: the column it is about by its place, which the
/// caller names (its table and the column's name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnError {
    /// Text past `u32::MAX` bytes: offsets are `u32`.
    TextTooLarge { bytes: usize },
    /// Offsets that do not start at 0, go down, end past their values or (for text) fall inside
    /// a character.
    Offsets { why: &'static str },
    /// A dictionary key, at `row`, past the dictionary's `values`.
    DictKey { row: usize, key: u32, values: usize },
    /// A validity of `len` rows for a column of `rows`.
    Validity { len: usize, rows: usize },
    /// The batch's `fields` and `columns` counts differ.
    Columns { fields: usize, columns: usize },
    /// The column at `at` is of `column`, its field of `field`.
    Type { at: usize, field: Type, column: Type },
    /// The column at `at` has `rows`, the batch `want`.
    Rows { at: usize, rows: usize, want: usize },
    /// The column at `at` has a null row, and its field is not nullable.
    Null { at: usize, row: usize },
}

impl fmt::Display for ColumnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnError::TextTooLarge { bytes } => {
                write!(f, "a text column of {bytes} bytes, past the 4 GiB a column's offsets reach")
            }
            ColumnError::Offsets { why } => write!(f, "a column's offsets {why}"),
            ColumnError::DictKey { row, key, values } => {
                write!(f, "row {row}'s dictionary key {key} is past its {values} values")
            }
            ColumnError::Validity { len, rows } => {
                write!(f, "a validity of {len} rows for a column of {rows}")
            }
            ColumnError::Columns { fields, columns } => {
                write!(f, "a batch of {fields} fields and {columns} columns")
            }
            ColumnError::Type { at, field, column } => {
                write!(f, "column {at} is {column:?}, its field {field:?}")
            }
            ColumnError::Rows { at, rows, want } => {
                write!(f, "column {at} has {rows} rows, the batch {want}")
            }
            ColumnError::Null { at, row } => {
                write!(f, "column {at} is not nullable, and its row {row} is null")
            }
        }
    }
}

impl core::error::Error for ColumnError {}

/// Text values end to end in one string, each from its offset to the next.
pub struct Texts<A: Allocator> {
    /// One more than the values: 0, then each value's end.
    offsets: Vec<u32, A>,
    text: String<A>,
}

impl<A: Allocator + Clone> Texts<A> {
    pub fn new_in(alloc: A) -> Texts<A> {
        Texts::with_capacity_in(0, 0, alloc)
    }

    /// Room for `values` values of `bytes` bytes in all.
    pub fn with_capacity_in(values: usize, bytes: usize, alloc: A) -> Texts<A> {
        let mut offsets = Vec::with_capacity_in(values + 1, alloc.clone());
        offsets.push(0);
        Texts { offsets, text: String::with_capacity_in(bytes, alloc) }
    }

    /// The texts of `offsets` (0 first, then each value's end) into `text`; refused unless
    /// every offset is in order and on a character's boundary.
    pub fn from_parts(offsets: Vec<u32, A>, text: String<A>) -> Result<Texts<A>, ColumnError> {
        if offsets.first() != Some(&0) {
            return Err(ColumnError::Offsets { why: "do not start at 0" });
        }
        let mut last = 0;
        for &end in offsets.iter() {
            if end < last {
                return Err(ColumnError::Offsets { why: "go down" });
            }
            let at = end.widen_to::<usize>();
            if at > text.len() {
                return Err(ColumnError::Offsets { why: "end past their text" });
            }
            if !text.as_str().is_char_boundary(at) {
                return Err(ColumnError::Offsets { why: "fall inside a character" });
            }
            last = end;
        }
        if last.widen_to::<usize>() != text.len() {
            return Err(ColumnError::Offsets { why: "end before their text does" });
        }
        Ok(Texts { offsets, text })
    }
}

impl<A: Allocator> Texts<A> {
    /// Adds `value`, or refuses past `u32::MAX` bytes in all.
    pub fn try_push(&mut self, value: &str) -> Result<(), ColumnError> {
        let bytes = self.text.len() + value.len();
        let end = u32::try_from(bytes).map_err(|_| ColumnError::TextTooLarge { bytes })?;
        self.text.push_str(value);
        self.offsets.push(end);
        Ok(())
    }

    /// Adds `value`; a column past 4 GiB of text is a bug of its maker, which panics (failing
    /// its own job).
    pub fn push(&mut self, value: &str) {
        if let Err(error) = self.try_push(value) {
            panic!("{error}");
        }
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `i`th value; panics past the end, as a slice does.
    pub fn get(&self, i: usize) -> &str {
        let (start, end) = (self.offsets[i].widen_to::<usize>(), self.offsets[i + 1].widen_to::<usize>());
        &self.text.as_str()[start..end]
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    /// 0, then each value's end.
    pub fn offsets(&self) -> &[u32] {
        self.offsets.as_slice()
    }

    /// Every value end to end.
    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    pub fn clear(&mut self) {
        self.offsets.truncate(1);
        self.text.clear();
    }
}

impl<A: Allocator + Clone> Clone for Texts<A> {
    fn clone(&self) -> Texts<A> {
        // clone: a column's texts, as its batch is cloned
        Texts { offsets: self.offsets.clone(), text: self.text.clone() }
    }
}

impl<A: Allocator> PartialEq for Texts<A> {
    fn eq(&self, other: &Texts<A>) -> bool {
        self.offsets.as_slice() == other.offsets.as_slice() && self.text.as_str() == other.text.as_str()
    }
}

impl<A: Allocator> fmt::Debug for Texts<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A column's values, one buffer of one type.
pub enum Values<A: Allocator> {
    Bool(Vec<bool, A>),
    I32(Vec<i32, A>),
    I64(Vec<i64, A>),
    U16(Vec<u16, A>),
    U32(Vec<u32, A>),
    U64(Vec<u64, A>),
    F64(Vec<f64, A>),
    Text(Texts<A>),
    /// Each value a key into `values`.
    Dict { keys: Vec<u32, A>, values: Texts<A> },
}

impl<A: Allocator> Values<A> {
    pub fn scalar(&self) -> Scalar {
        match self {
            Values::Bool(_) => Scalar::Bool,
            Values::I32(_) => Scalar::I32,
            Values::I64(_) => Scalar::I64,
            Values::U16(_) => Scalar::U16,
            Values::U32(_) => Scalar::U32,
            Values::U64(_) => Scalar::U64,
            Values::F64(_) => Scalar::F64,
            Values::Text(_) => Scalar::Text,
            Values::Dict { .. } => Scalar::Dict,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Values::Bool(it) => it.len(),
            Values::I32(it) => it.len(),
            Values::I64(it) => it.len(),
            Values::U16(it) => it.len(),
            Values::U32(it) => it.len(),
            Values::U64(it) => it.len(),
            Values::F64(it) => it.len(),
            Values::Text(it) => it.len(),
            Values::Dict { keys, .. } => keys.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The values as text, plain or keyed; `None` for numbers.
    pub fn strs(&self) -> Option<Strs<'_, A>> {
        match self {
            Values::Text(texts) => Some(Strs { keys: None, texts, valid: None }),
            Values::Dict { keys, values } => {
                Some(Strs { keys: Some(keys.as_slice()), texts: values, valid: None })
            }
            _ => None,
        }
    }

    /// The values as `T`s; `None` when they are of another type.
    pub fn slice<T: Native>(&self) -> Option<&[T]> {
        T::slice(self)
    }

    /// Every dictionary key below its values' count, or the first that is not; `valid` says
    /// which rows' keys are read (`None`: all).
    fn check_keys(&self, valid: Option<&[bool]>) -> Result<(), ColumnError> {
        let Values::Dict { keys, values } = self else { return Ok(()) };
        for (row, &key) in keys.iter().enumerate() {
            let read = valid.is_none_or(|it| it[row]);
            if read && key.widen_to::<usize>() >= values.len() {
                return Err(ColumnError::DictKey { row, key, values: values.len() });
            }
        }
        Ok(())
    }
}

impl<A: Allocator + Clone> Clone for Values<A> {
    fn clone(&self) -> Values<A> {
        // clone: a column's values, as its batch is cloned
        match self {
            Values::Bool(it) => Values::Bool(it.clone()),
            Values::I32(it) => Values::I32(it.clone()),
            Values::I64(it) => Values::I64(it.clone()),
            Values::U16(it) => Values::U16(it.clone()),
            Values::U32(it) => Values::U32(it.clone()),
            Values::U64(it) => Values::U64(it.clone()),
            Values::F64(it) => Values::F64(it.clone()),
            Values::Text(it) => Values::Text(it.clone()),
            Values::Dict { keys, values } => Values::Dict { keys: keys.clone(), values: values.clone() },
        }
    }
}

impl<A: Allocator> PartialEq for Values<A> {
    /// Equal values of equal types: a dictionary column equals another only keyed alike.
    fn eq(&self, other: &Values<A>) -> bool {
        match (self, other) {
            (Values::Bool(a), Values::Bool(b)) => a.as_slice() == b.as_slice(),
            (Values::I32(a), Values::I32(b)) => a.as_slice() == b.as_slice(),
            (Values::I64(a), Values::I64(b)) => a.as_slice() == b.as_slice(),
            (Values::U16(a), Values::U16(b)) => a.as_slice() == b.as_slice(),
            (Values::U32(a), Values::U32(b)) => a.as_slice() == b.as_slice(),
            (Values::U64(a), Values::U64(b)) => a.as_slice() == b.as_slice(),
            (Values::F64(a), Values::F64(b)) => a.as_slice() == b.as_slice(),
            (Values::Text(a), Values::Text(b)) => a == b,
            (Values::Dict { keys: a, values: x }, Values::Dict { keys: b, values: y }) => {
                a.as_slice() == b.as_slice() && x == y
            }
            _ => false,
        }
    }
}

impl<A: Allocator> fmt::Debug for Values<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Values::Bool(it) => f.debug_tuple("Bool").field(&it.as_slice()).finish(),
            Values::I32(it) => f.debug_tuple("I32").field(&it.as_slice()).finish(),
            Values::I64(it) => f.debug_tuple("I64").field(&it.as_slice()).finish(),
            Values::U16(it) => f.debug_tuple("U16").field(&it.as_slice()).finish(),
            Values::U32(it) => f.debug_tuple("U32").field(&it.as_slice()).finish(),
            Values::U64(it) => f.debug_tuple("U64").field(&it.as_slice()).finish(),
            Values::F64(it) => f.debug_tuple("F64").field(&it.as_slice()).finish(),
            Values::Text(it) => f.debug_tuple("Text").field(it).finish(),
            Values::Dict { keys, values } => {
                f.debug_struct("Dict").field("keys", &keys.as_slice()).field("values", values).finish()
            }
        }
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A number type a column holds.
pub trait Native: Copy + PartialOrd + fmt::Debug + Send + Sync + 'static + sealed::Sealed {
    const SCALAR: Scalar;
    /// What a null row holds.
    const ZERO: Self;
    fn wrap<A: Allocator>(values: Vec<Self, A>) -> Values<A>;
    fn slice<A: Allocator>(values: &Values<A>) -> Option<&[Self]>;
}

macro_rules! native {
    ($($t:ty => $v:ident, $zero:expr;)*) => {$(
        impl sealed::Sealed for $t {}

        impl Native for $t {
            const SCALAR: Scalar = Scalar::$v;
            const ZERO: $t = $zero;

            fn wrap<A: Allocator>(values: Vec<$t, A>) -> Values<A> {
                Values::$v(values)
            }

            fn slice<A: Allocator>(values: &Values<A>) -> Option<&[$t]> {
                match values {
                    Values::$v(it) => Some(it.as_slice()),
                    _ => None,
                }
            }
        }
    )*};
}

native! {
    bool => Bool, false;
    i32 => I32, 0;
    i64 => I64, 0;
    u16 => U16, 0;
    u32 => U32, 0;
    u64 => U64, 0;
    f64 => F64, 0.0;
}

/// One column: its values, which rows are null, and for a list column each row's elements.
pub struct Column<A: Allocator> {
    values: Values<A>,
    /// `None` when no row is null.
    valid: Option<Vec<bool, A>>,
    /// A list column's offsets into `values`: 0, then each row's end.
    offsets: Option<Vec<u32, A>>,
}

impl<A: Allocator + Clone> Column<A> {
    /// A column of `values`, none null.
    pub fn values<T: Native>(values: impl IntoIterator<Item = T>, alloc: A) -> Column<A> {
        Column { values: T::wrap(Vec::from_iter_in(values, alloc)), valid: None, offsets: None }
    }

    /// A column of `values`, null where `None`.
    pub fn options<T: Native>(values: impl IntoIterator<Item = Option<T>>, alloc: A) -> Column<A> {
        let values = values.into_iter();
        let rows = values.size_hint().0;
        let mut out = Vec::with_capacity_in(rows, alloc.clone());
        let mut valid = Vec::with_capacity_in(rows, alloc);
        for value in values {
            valid.push(value.is_some());
            out.push(value.unwrap_or(T::ZERO));
        }
        Column::with_validity(T::wrap(out), valid)
    }

    /// `value`, `rows` times.
    pub fn repeat<T: Native>(value: T, rows: usize, alloc: A) -> Column<A> {
        Column { values: T::wrap(Vec::from_elem_in(value, rows, alloc)), valid: None, offsets: None }
    }

    /// `rows` nulls of `ty`.
    pub fn nulls(ty: Type, rows: usize, alloc: A) -> Column<A> {
        let valid = Vec::from_elem_in(false, rows, alloc.clone());
        let values = match ty {
            Type::List(scalar) => {
                let offsets = Vec::from_elem_in(0u32, rows + 1, alloc.clone());
                let values = empty(scalar, alloc);
                return Column { values, valid: Some(valid), offsets: Some(offsets) };
            }
            Type::One(Scalar::Text) => {
                let mut texts = Texts::with_capacity_in(rows, 0, alloc);
                for _ in 0..rows {
                    texts.push("");
                }
                Values::Text(texts)
            }
            Type::One(Scalar::Dict) => {
                Values::Dict { keys: Vec::from_elem_in(0, rows, alloc.clone()), values: Texts::new_in(alloc) }
            }
            Type::One(Scalar::Bool) => Values::Bool(Vec::from_elem_in(false, rows, alloc)),
            Type::One(Scalar::I32) => Values::I32(Vec::from_elem_in(0, rows, alloc)),
            Type::One(Scalar::I64) => Values::I64(Vec::from_elem_in(0, rows, alloc)),
            Type::One(Scalar::U16) => Values::U16(Vec::from_elem_in(0, rows, alloc)),
            Type::One(Scalar::U32) => Values::U32(Vec::from_elem_in(0, rows, alloc)),
            Type::One(Scalar::U64) => Values::U64(Vec::from_elem_in(0, rows, alloc)),
            Type::One(Scalar::F64) => Values::F64(Vec::from_elem_in(0.0, rows, alloc)),
        };
        Column { values, valid: Some(valid), offsets: None }
    }

    /// A text column of `texts`, none null, its bytes allocated once at their size.
    pub fn texts<S: AsRef<str>>(texts: &[S], alloc: A) -> Column<A> {
        let bytes = texts.iter().map(|it| it.as_ref().len()).sum();
        let mut out = Texts::with_capacity_in(texts.len(), bytes, alloc);
        for text in texts {
            out.push(text.as_ref());
        }
        Column { values: Values::Text(out), valid: None, offsets: None }
    }

    /// A text column of `texts`, null where `None`, its bytes allocated once at their size.
    pub fn opt_texts<S: AsRef<str>>(texts: &[Option<S>], alloc: A) -> Column<A> {
        let bytes = texts.iter().map(|it| it.as_ref().map_or(0, |it| it.as_ref().len())).sum();
        let mut out = Texts::with_capacity_in(texts.len(), bytes, alloc.clone());
        let mut valid = Vec::with_capacity_in(texts.len(), alloc);
        for text in texts {
            valid.push(text.is_some());
            out.push(text.as_ref().map_or("", |it| it.as_ref()));
        }
        Column::with_validity(Values::Text(out), valid)
    }

    /// A text column of the texts an iterator gives, none null.
    pub fn texts_of<'s>(texts: impl IntoIterator<Item = &'s str>, alloc: A) -> Column<A> {
        let mut out = Texts::new_in(alloc);
        for text in texts {
            out.push(text);
        }
        Column { values: Values::Text(out), valid: None, offsets: None }
    }

    /// A text column of the texts an iterator gives, null where `None`.
    pub fn opt_texts_of<'s>(texts: impl IntoIterator<Item = Option<&'s str>>, alloc: A) -> Column<A> {
        let mut out = Texts::new_in(alloc.clone());
        let mut valid = Vec::new_in(alloc);
        for text in texts {
            valid.push(text.is_some());
            out.push(text.unwrap_or(""));
        }
        Column::with_validity(Values::Text(out), valid)
    }

    /// A dictionary column: each row a key into `values`, null where `keys` is `None`.
    pub fn dict(
        keys: impl IntoIterator<Item = Option<u32>>,
        values: Texts<A>,
        alloc: A,
    ) -> Result<Column<A>, ColumnError> {
        let keys = keys.into_iter();
        let rows = keys.size_hint().0;
        let mut out = Vec::with_capacity_in(rows, alloc.clone());
        let mut valid = Vec::with_capacity_in(rows, alloc);
        for key in keys {
            valid.push(key.is_some());
            out.push(key.unwrap_or(0));
        }
        let column = Column::with_validity(Values::Dict { keys: out, values }, valid);
        column.values.check_keys(column.validity())?;
        Ok(column)
    }

    /// The column of `values`, null where `valid` says (`None`: none), refused when they
    /// differ in length or a dictionary key is past its values.
    pub fn try_new(values: Values<A>, valid: Option<Vec<bool, A>>) -> Result<Column<A>, ColumnError> {
        if let Some(it) = &valid
            && it.len() != values.len()
        {
            return Err(ColumnError::Validity { len: it.len(), rows: values.len() });
        }
        values.check_keys(valid.as_ref().map(|it| it.as_slice()))?;
        Ok(match valid {
            Some(valid) => Column::with_validity(values, valid),
            None => Column { values, valid: None, offsets: None },
        })
    }

    /// A list column: row `i`'s elements are `values[offsets[i]..offsets[i + 1]]`, never
    /// null; the row null where `valid` says (`None`: none).
    pub fn list(
        offsets: Vec<u32, A>,
        valid: Option<Vec<bool, A>>,
        values: Values<A>,
    ) -> Result<Column<A>, ColumnError> {
        if offsets.first() != Some(&0) {
            return Err(ColumnError::Offsets { why: "do not start at 0" });
        }
        if offsets.windows(2).any(|it| it[1] < it[0]) {
            return Err(ColumnError::Offsets { why: "go down" });
        }
        if offsets.last().map(|it| it.widen_to::<usize>()) != Some(values.len()) {
            return Err(ColumnError::Offsets { why: "do not end at their values' end" });
        }
        let rows = offsets.len() - 1;
        if let Some(it) = &valid
            && it.len() != rows
        {
            return Err(ColumnError::Validity { len: it.len(), rows });
        }
        values.check_keys(None)?;
        let valid = valid.filter(|it| it.contains(&false));
        Ok(Column { values, valid, offsets: Some(offsets) })
    }

    fn with_validity(values: Values<A>, valid: Vec<bool, A>) -> Column<A> {
        let valid = Some(valid).filter(|it| it.contains(&false));
        Column { values, valid, offsets: None }
    }
}

/// No values of `scalar`.
pub fn empty<A: Allocator + Clone>(scalar: Scalar, alloc: A) -> Values<A> {
    match scalar {
        Scalar::Bool => Values::Bool(Vec::new_in(alloc)),
        Scalar::I32 => Values::I32(Vec::new_in(alloc)),
        Scalar::I64 => Values::I64(Vec::new_in(alloc)),
        Scalar::U16 => Values::U16(Vec::new_in(alloc)),
        Scalar::U32 => Values::U32(Vec::new_in(alloc)),
        Scalar::U64 => Values::U64(Vec::new_in(alloc)),
        Scalar::F64 => Values::F64(Vec::new_in(alloc)),
        Scalar::Text => Values::Text(Texts::new_in(alloc)),
        Scalar::Dict => Values::Dict { keys: Vec::new_in(alloc.clone()), values: Texts::new_in(alloc) },
    }
}

impl<A: Allocator> Column<A> {
    /// Its rows.
    pub fn len(&self) -> usize {
        match &self.offsets {
            Some(it) => it.len() - 1,
            None => self.values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn ty(&self) -> Type {
        match self.offsets {
            Some(_) => Type::List(self.values.scalar()),
            None => Type::One(self.values.scalar()),
        }
    }

    /// Its values: a row each, or a list column's elements.
    pub fn buffer(&self) -> &Values<A> {
        &self.values
    }

    /// Which rows are valid (not null); `None` when every one is.
    pub fn validity(&self) -> Option<&[bool]> {
        self.valid.as_ref().map(|it| it.as_slice())
    }

    /// A list column's offsets into its values: 0, then each row's end.
    pub fn list_offsets(&self) -> Option<&[u32]> {
        self.offsets.as_ref().map(|it| it.as_slice())
    }

    pub fn is_null(&self, row: usize) -> bool {
        self.valid.as_ref().is_some_and(|it| !it[row])
    }

    pub fn is_valid(&self, row: usize) -> bool {
        !self.is_null(row)
    }

    pub fn null_count(&self) -> usize {
        self.valid.as_ref().map_or(0, |it| it.iter().filter(|it| !**it).count())
    }

    /// Its parts: values, validity, list offsets.
    pub fn into_parts(self) -> (Values<A>, Option<Vec<bool, A>>, Option<Vec<u32, A>>) {
        (self.values, self.valid, self.offsets)
    }

    /// Its rows as `T`s; `None` unless it is a column (not a list) of `T`.
    pub fn prim<T: Native>(&self) -> Option<Prim<'_, T>> {
        if self.offsets.is_some() {
            return None;
        }
        Some(Prim { values: T::slice(&self.values)?, valid: self.validity() })
    }

    pub fn bools(&self) -> Option<Prim<'_, bool>> {
        self.prim()
    }

    pub fn i32s(&self) -> Option<Prim<'_, i32>> {
        self.prim()
    }

    pub fn i64s(&self) -> Option<Prim<'_, i64>> {
        self.prim()
    }

    pub fn u16s(&self) -> Option<Prim<'_, u16>> {
        self.prim()
    }

    pub fn u32s(&self) -> Option<Prim<'_, u32>> {
        self.prim()
    }

    pub fn u64s(&self) -> Option<Prim<'_, u64>> {
        self.prim()
    }

    pub fn f64s(&self) -> Option<Prim<'_, f64>> {
        self.prim()
    }

    /// Its rows as text, plain or keyed; `None` unless it is a text column (not a list).
    pub fn strs(&self) -> Option<Strs<'_, A>> {
        if self.offsets.is_some() {
            return None;
        }
        let mut strs = self.values.strs()?;
        strs.valid = self.validity();
        Some(strs)
    }

    /// Its rows as lists; `None` unless it is a list column.
    pub fn lists(&self) -> Option<Lists<'_, A>> {
        let offsets = self.offsets.as_ref()?.as_slice();
        Some(Lists { offsets, valid: self.validity(), values: &self.values })
    }

    /// Row `row` of an integer column, of whichever width and sign; `None` where null or not
    /// an integer column.
    pub fn int(&self, row: usize) -> Option<i128> {
        if self.offsets.is_some() || self.is_null(row) {
            return None;
        }
        match &self.values {
            Values::I32(it) => Some(it[row].into()),
            Values::I64(it) => Some(it[row].into()),
            Values::U16(it) => Some(it[row].into()),
            Values::U32(it) => Some(it[row].into()),
            Values::U64(it) => Some(it[row].into()),
            _ => None,
        }
    }

    /// Row `row` of a number column as a double (an integer the nearest double, as SQL casts
    /// it); `None` where null or not a number column.
    #[expect(
        blaze::as_cast,
        reason = "an integer to its nearest double, the one conversion `as` names for it"
    )]
    pub fn float(&self, row: usize) -> Option<f64> {
        if self.offsets.is_some() || self.is_null(row) {
            return None;
        }
        match &self.values {
            Values::F64(it) => Some(it[row]),
            _ => self.int(row).map(|it| it as f64),
        }
    }

    /// Row `row` of a text column; `None` where null or not a text column.
    pub fn text(&self, row: usize) -> Option<&str> {
        self.strs()?.get(row)
    }
}

impl<A: Allocator + Clone> Clone for Column<A> {
    fn clone(&self) -> Column<A> {
        // clone: a column, as its batch is cloned
        Column { values: self.values.clone(), valid: self.valid.clone(), offsets: self.offsets.clone() }
    }
}

impl<A: Allocator> PartialEq for Column<A> {
    fn eq(&self, other: &Column<A>) -> bool {
        self.values == other.values
            && self.validity() == other.validity()
            && self.list_offsets() == other.list_offsets()
    }
}

impl<A: Allocator> fmt::Debug for Column<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Column")
            .field("values", &self.values)
            .field("valid", &self.validity())
            .field("offsets", &self.list_offsets())
            .finish()
    }
}

/// A number column's rows.
#[derive(Clone, Copy, Debug)]
pub struct Prim<'a, T> {
    values: &'a [T],
    valid: Option<&'a [bool]>,
}

impl<'a, T: Native> Prim<'a, T> {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn is_null(&self, row: usize) -> bool {
        self.valid.is_some_and(|it| !it[row])
    }

    pub fn is_valid(&self, row: usize) -> bool {
        !self.is_null(row)
    }

    /// Row `row`'s value, null or not (a null row's is unspecified).
    pub fn value(&self, row: usize) -> T {
        self.values[row]
    }

    /// Row `row`'s value; `None` where null.
    pub fn get(&self, row: usize) -> Option<T> {
        (!self.is_null(row)).then(|| self.values[row])
    }

    /// Every row's value, null or not.
    pub fn values(&self) -> &'a [T] {
        self.values
    }

    pub fn validity(&self) -> Option<&'a [bool]> {
        self.valid
    }

    /// Each row's value, `None` where null.
    pub fn iter(&self) -> impl Iterator<Item = Option<T>> + 'a {
        let (values, valid) = (self.values, self.valid);
        (0..values.len()).map(move |row| valid.is_none_or(|it| it[row]).then(|| values[row]))
    }
}

/// A text column's rows, plain or keyed.
pub struct Strs<'a, A: Allocator> {
    keys: Option<&'a [u32]>,
    texts: &'a Texts<A>,
    valid: Option<&'a [bool]>,
}

impl<A: Allocator> Clone for Strs<'_, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A: Allocator> Copy for Strs<'_, A> {}

impl<'a, A: Allocator> Strs<'a, A> {
    pub fn len(&self) -> usize {
        self.keys.map_or(self.texts.len(), <[u32]>::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_null(&self, row: usize) -> bool {
        self.valid.is_some_and(|it| !it[row])
    }

    /// Row `row`'s text; `None` where null.
    pub fn get(&self, row: usize) -> Option<&'a str> {
        (!self.is_null(row)).then(|| self.value(row))
    }

    /// Row `row`'s text, `""` where null.
    pub fn value(&self, row: usize) -> &'a str {
        if self.is_null(row) {
            return "";
        }
        match self.keys {
            Some(keys) => self.texts.get(keys[row].widen_to::<usize>()),
            None => self.texts.get(row),
        }
    }

    /// Each row's text, `None` where null.
    pub fn iter(&self) -> impl Iterator<Item = Option<&'a str>> + 'a {
        let this = *self;
        (0..this.len()).map(move |row| this.get(row))
    }

    /// A dictionary column's keys and values; `None` for plain text.
    pub fn dict(&self) -> Option<(&'a [u32], &'a Texts<A>)> {
        Some((self.keys?, self.texts))
    }

    /// A plain text column's texts; `None` for a dictionary column.
    pub fn plain(&self) -> Option<&'a Texts<A>> {
        self.keys.is_none().then_some(self.texts)
    }
}

/// A list column's rows.
pub struct Lists<'a, A: Allocator> {
    offsets: &'a [u32],
    valid: Option<&'a [bool]>,
    values: &'a Values<A>,
}

impl<A: Allocator> Clone for Lists<'_, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A: Allocator> Copy for Lists<'_, A> {}

impl<'a, A: Allocator> Lists<'a, A> {
    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_null(&self, row: usize) -> bool {
        self.valid.is_some_and(|it| !it[row])
    }

    /// Row `row`'s elements' place in [`Lists::elements`]; `None` where null.
    pub fn row(&self, row: usize) -> Option<Range<usize>> {
        (!self.is_null(row))
            .then(|| self.offsets[row].widen_to::<usize>()..self.offsets[row + 1].widen_to::<usize>())
    }

    /// Every row's elements, end to end.
    pub fn elements(&self) -> &'a Values<A> {
        self.values
    }

    /// The elements as text; `None` for lists of numbers.
    pub fn element_strs(&self) -> Option<Strs<'a, A>> {
        self.values.strs()
    }

    /// The elements as `T`s; `None` for lists of another type.
    pub fn element_slice<T: Native>(&self) -> Option<&'a [T]> {
        T::slice(self.values)
    }

    /// Row `row`'s texts; `None` where null or a list of numbers.
    pub fn texts(&self, row: usize) -> Option<impl Iterator<Item = &'a str> + 'a> {
        let range = self.row(row)?;
        let strs = self.element_strs()?;
        Some(range.map(move |i| strs.value(i)))
    }

    /// Row `row`'s `T`s; `None` where null or a list of another type.
    pub fn slice<T: Native>(&self, row: usize) -> Option<&'a [T]> {
        let range = self.row(row)?;
        Some(&self.element_slice::<T>()?[range])
    }
}

/// Named columns of one length.
pub struct Batch<A: Allocator> {
    fields: Vec<Field<A>, A>,
    columns: Vec<Column<A>, A>,
    rows: usize,
}

impl<A: Allocator> Batch<A> {
    /// The batch of `columns`, named and typed by `fields`: refused unless each column is of
    /// its field's type, every column has the first's rows, and a column whose field is not
    /// nullable has no null row. A batch of no columns has no rows.
    pub fn try_new(fields: Vec<Field<A>, A>, columns: Vec<Column<A>, A>) -> Result<Batch<A>, ColumnError> {
        if fields.len() != columns.len() {
            return Err(ColumnError::Columns { fields: fields.len(), columns: columns.len() });
        }
        let rows = columns.first().map_or(0, Column::len);
        for (at, (field, column)) in fields.iter().zip(columns.iter()).enumerate() {
            if field.ty != column.ty() {
                return Err(ColumnError::Type { at, field: field.ty, column: column.ty() });
            }
            if column.len() != rows {
                return Err(ColumnError::Rows { at, rows: column.len(), want: rows });
            }
            if !field.nullable
                && let Some(row) = column.validity().and_then(|it| it.iter().position(|it| !*it))
            {
                return Err(ColumnError::Null { at, row });
            }
        }
        Ok(Batch { fields, columns, rows })
    }

    pub fn num_rows(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    pub fn fields(&self) -> &[Field<A>] {
        self.fields.as_slice()
    }

    pub fn columns(&self) -> &[Column<A>] {
        self.columns.as_slice()
    }

    pub fn field(&self, at: usize) -> &Field<A> {
        &self.fields[at]
    }

    pub fn column(&self, at: usize) -> &Column<A> {
        &self.columns[at]
    }

    /// The place of the column named `name`.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|it| it.name() == name)
    }

    pub fn column_by_name(&self, name: &str) -> Option<&Column<A>> {
        self.index_of(name).map(|at| &self.columns[at])
    }

    pub fn into_parts(self) -> (Vec<Field<A>, A>, Vec<Column<A>, A>) {
        (self.fields, self.columns)
    }
}

impl<A: Allocator + Clone> Clone for Batch<A> {
    fn clone(&self) -> Batch<A> {
        // clone: a batch, at its holder's ask
        Batch { fields: self.fields.clone(), columns: self.columns.clone(), rows: self.rows }
    }
}

impl<A: Allocator> PartialEq for Batch<A> {
    fn eq(&self, other: &Batch<A>) -> bool {
        self.rows == other.rows
            && self.fields.as_slice() == other.fields.as_slice()
            && self.columns.as_slice() == other.columns.as_slice()
    }
}

impl<A: Allocator> fmt::Debug for Batch<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Batch")
            .field("rows", &self.rows)
            .field("fields", &self.fields.as_slice())
            .field("columns", &self.columns.as_slice())
            .finish()
    }
}
