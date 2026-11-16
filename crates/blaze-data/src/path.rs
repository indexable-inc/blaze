//! Typed reading: a [`Cursor`] is a value and the path to it, and every refusal is a
//! [`ShapeError`] naming that path: `a.b[3]: expected text, found integer`.
//!
//! The path is a chain of [`At`]s on the stack, one per step down, and is written out only when
//! an error is made, into the cursor's allocator; reading a value that fits allocates nothing
//! for the path.

use core::{alloc::Allocator, fmt, fmt::Write as _};

use blaze_own::String;

use crate::{FromValue, Kind, Map, Value};

/// One step down from the root.
#[derive(Clone, Copy, Debug)]
pub enum Step<'p> {
    Root,
    Key(&'p str),
    Index(usize),
}

/// Where a value is: its step and the place it was stepped from.
#[derive(Clone, Copy, Debug)]
pub struct At<'p> {
    parent: Option<&'p At<'p>>,
    step: Step<'p>,
}

impl<'p> At<'p> {
    pub const ROOT: At<'static> = At { parent: None, step: Step::Root };

    pub fn key(&'p self, key: &'p str) -> At<'p> {
        At { parent: Some(self), step: Step::Key(key) }
    }

    pub fn index(&'p self, index: usize) -> At<'p> {
        At { parent: Some(self), step: Step::Index(index) }
    }

    /// The path as `a.b[3]`; a key that is not `[A-Za-z0-9_-]+` is quoted (`a."x.y"`); the
    /// root alone is empty.
    pub fn write_into<W: fmt::Write + ?Sized>(&self, out: &mut W) -> fmt::Result {
        self.write_steps(out).map(|_| ())
    }

    /// Writes the steps, and says whether any was written.
    fn write_steps<W: fmt::Write + ?Sized>(&self, out: &mut W) -> Result<bool, fmt::Error> {
        let before = match self.parent {
            Some(parent) => parent.write_steps(out)?,
            None => false,
        };
        write_step(out, self.step, before)
    }

    fn render_in<A: Allocator>(&self, alloc: A) -> String<A> {
        let mut out = String::new_in(alloc);
        if self.write_into(&mut out).is_err() {
            unreachable!("writing into a `String` does not fail");
        }
        out
    }
}

/// One step of a path written as [`At::write_into`] writes it, after `before` (whether a step
/// was written already); says whether anything is written now. A writer that walks a value
/// with its own stack, not a chain of `At`s, writes its path step by step with this.
pub fn write_step<W: fmt::Write + ?Sized>(out: &mut W, step: Step<'_>, before: bool) -> Result<bool, fmt::Error> {
    match step {
        Step::Root => Ok(before),
        Step::Key(key) => {
            if before {
                out.write_char('.')?;
            }
            let bare = !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            match bare {
                true => out.write_str(key)?,
                false => write!(out, "{key:?}")?,
            }
            Ok(true)
        }
        Step::Index(index) => {
            write!(out, "[{index}]")?;
            Ok(true)
        }
    }
}

/// Why a value does not fit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    /// The value is a `found` where a `expected` was wanted.
    Expected { expected: Kind, found: Kind },
    /// A field that must be there is not; the path ends in its name.
    Missing,
    /// A field no one asked for; the path ends in its name.
    Unknown,
    /// An integer outside the type's range (`300` for a `u8`), or a float an integer type
    /// cannot hold; `ty` is the type's name.
    OutOfRange { ty: &'static str },
    /// An index past a list of `len`.
    NoIndex { len: usize },
}

/// A value that does not fit: the path to it and why.
pub struct ShapeError<A: Allocator> {
    pub path: String<A>,
    pub shape: Shape,
}

impl<A: Allocator> fmt::Display for ShapeError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = match self.path.is_empty() {
            true => "the value",
            false => self.path.as_str(),
        };
        match self.shape {
            Shape::Expected { expected, found } => {
                write!(f, "{path}: expected {expected}, found {found}")
            }
            Shape::Missing => write!(f, "{path}: missing"),
            Shape::Unknown => write!(f, "{path}: no such field"),
            Shape::OutOfRange { ty } => write!(f, "{path}: out of range for {ty}"),
            Shape::NoIndex { len } => write!(f, "{path}: past the end of a list of {len}"),
        }
    }
}

impl<A: Allocator> fmt::Debug for ShapeError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShapeError({self})")
    }
}

impl<A: Allocator> core::error::Error for ShapeError<A> {}

/// A value, the path to it, and the allocator errors and read values are made in.
pub struct Cursor<'v, 'p, A: Allocator + Clone> {
    value: &'v Value<A>,
    at: At<'p>,
    alloc: A,
}

impl<'v, 'p, A: Allocator + Clone> Cursor<'v, 'p, A> {
    /// The root of `value`.
    pub fn root(value: &'v Value<A>, alloc: A) -> Cursor<'v, 'static, A> {
        Cursor { value, at: At::ROOT, alloc }
    }

    pub fn value(&self) -> &'v Value<A> {
        self.value
    }

    pub fn at(&self) -> &At<'p> {
        &self.at
    }

    pub fn alloc(&self) -> &A {
        &self.alloc
    }

    /// An error about this value.
    pub fn error(&self, shape: Shape) -> ShapeError<A> {
        ShapeError { path: self.at.render_in(self.alloc.clone()), shape }
    }

    /// An error that this value is no `expected`.
    pub fn expected(&self, expected: Kind) -> ShapeError<A> {
        self.error(Shape::Expected { expected, found: self.value.kind() })
    }

    /// This value read as a `T`.
    pub fn get<T: FromValue<A>>(&self) -> Result<T, ShapeError<A>> {
        T::from_value(self)
    }

    pub fn is_null(&self) -> bool {
        self.value.is_null()
    }

    pub fn bool(&self) -> Result<bool, ShapeError<A>> {
        self.value.as_bool().ok_or_else(|| self.expected(Kind::Bool))
    }

    pub fn text(&self) -> Result<&'v str, ShapeError<A>> {
        self.value.as_str().ok_or_else(|| self.expected(Kind::Text))
    }

    pub fn bytes(&self) -> Result<&'v [u8], ShapeError<A>> {
        self.value.as_bytes().ok_or_else(|| self.expected(Kind::Bytes))
    }

    pub fn list(&self) -> Result<&'v [Value<A>], ShapeError<A>> {
        self.value.as_list().ok_or_else(|| self.expected(Kind::List))
    }

    pub fn map(&self) -> Result<&'v Map<A>, ShapeError<A>> {
        self.value.as_map().ok_or_else(|| self.expected(Kind::Map))
    }

    /// The field `key` of this map, which must be there.
    pub fn field<'q>(&'q self, key: &'q str) -> Result<Cursor<'v, 'q, A>, ShapeError<A>> {
        let map = self.map()?;
        let at = self.at.key(key);
        match map.get(key) {
            Some(value) => Ok(Cursor { value, at, alloc: self.alloc.clone() }),
            None => Err(Cursor { value: self.value, at, alloc: self.alloc.clone() }.error(Shape::Missing)),
        }
    }

    /// The field `key` of this map, if it is there.
    pub fn field_opt<'q>(&'q self, key: &'q str) -> Result<Option<Cursor<'v, 'q, A>>, ShapeError<A>> {
        let map = self.map()?;
        Ok(map.get(key).map(|value| Cursor { value, at: self.at.key(key), alloc: self.alloc.clone() }))
    }

    /// The field `key` read as a `T`, which must be there.
    pub fn req<T: FromValue<A>>(&self, key: &str) -> Result<T, ShapeError<A>> {
        self.field(key)?.get()
    }

    /// The field `key` read as a `T`; `None` when it is missing or null.
    pub fn opt<T: FromValue<A>>(&self, key: &str) -> Result<Option<T>, ShapeError<A>> {
        match self.field_opt(key)? {
            Some(field) if !field.is_null() => field.get().map(Some),
            _ => Ok(None),
        }
    }

    /// Refuses a map with a key outside `known`, naming the first such key in insertion order.
    pub fn deny_unknown(&self, known: &[&str]) -> Result<(), ShapeError<A>> {
        let map = self.map()?;
        match map.iter().find(|(key, _)| !known.contains(key)) {
            Some((key, _)) => {
                let at = self.at.key(key);
                Err(Cursor { value: self.value, at, alloc: self.alloc.clone() }.error(Shape::Unknown))
            }
            None => Ok(()),
        }
    }

    /// The element `index` of this list.
    pub fn index<'q>(&'q self, index: usize) -> Result<Cursor<'v, 'q, A>, ShapeError<A>> {
        let list = self.list()?;
        match list.get(index) {
            Some(value) => Ok(Cursor { value, at: self.at.index(index), alloc: self.alloc.clone() }),
            None => Err(self.error(Shape::NoIndex { len: list.len() })),
        }
    }

    /// Each element of this list, in order, given to `each` as a cursor.
    pub fn each<E: From<ShapeError<A>>>(
        &self,
        mut each: impl FnMut(Cursor<'v, '_, A>) -> Result<(), E>,
    ) -> Result<(), E> {
        let list = self.list()?;
        for (i, value) in list.iter().enumerate() {
            each(Cursor { value, at: self.at.index(i), alloc: self.alloc.clone() })?;
        }
        Ok(())
    }

    /// Each entry of this map, in insertion order, given to `each` with its key.
    pub fn entries<E: From<ShapeError<A>>>(
        &self,
        mut each: impl FnMut(&'v str, Cursor<'v, '_, A>) -> Result<(), E>,
    ) -> Result<(), E> {
        let map = self.map()?;
        for (key, value) in map.iter() {
            each(key, Cursor { value, at: self.at.key(key), alloc: self.alloc.clone() })?;
        }
        Ok(())
    }
}
