//! Typed reading: a [`Cursor`] is a value and the path to it, and every refusal is a
//! [`ShapeError`] naming that path: `a.b[3]: expected text, found integer`.
//!
//! The path is a chain of [`At`]s on the stack, one per step down, and is written out only when
//! an error is made, into the cursor's allocator; reading a value that fits allocates nothing
//! for the path.

#![blaze::sans_io]

use core::{alloc::Allocator, cell::RefCell, fmt, fmt::Write as _};

use blaze_own::{String, Vec};

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
/// was written already); says whether anything is written now. A walk that keeps its own
/// stack (a [`Walk`](crate::Walk), a writer's table stack) and no chain of `At`s writes its
/// path step by step with this.
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
    /// `count` keys of the map at the path that its reader did not take; the error's `note`
    /// names each (with the closest known name when one is near) and the known ones.
    UnknownFields { count: usize },
    /// Text naming no variant; the error's `note` names it, the closest variant when one is
    /// near, and the known ones.
    UnknownVariant,
    /// An integer outside the type's range (`300` for a `u8`), or a float an integer type
    /// cannot hold; `ty` is the type's name.
    OutOfRange { ty: &'static str },
    /// An index past a list of `len`.
    NoIndex { len: usize },
    /// Text (or bytes) of the right kind that does not read as `ty` (`"1.x.3"` for a version);
    /// the error's `note` says why, in the type's own words.
    Invalid { ty: &'static str },
}

/// A value that does not fit: the path to it, why, and for an unknown name the names (empty
/// otherwise).
pub struct ShapeError<A: Allocator> {
    pub path: String<A>,
    pub shape: Shape,
    /// For [`Shape::UnknownFields`] and [`Shape::UnknownVariant`]: what follows the path, as
    /// `unknown field "colour" (did you mean "color"?); known: "color", "size"`.
    pub note: String<A>,
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
            Shape::Missing if self.note.is_empty() => write!(f, "{path}: missing"),
            Shape::Missing => write!(f, "{path}: missing ({})", self.note.as_str()),
            Shape::UnknownFields { .. } | Shape::UnknownVariant => write!(f, "{path}: {}", self.note.as_str()),
            Shape::OutOfRange { ty } => write!(f, "{path}: out of range for {ty}"),
            Shape::NoIndex { len } => write!(f, "{path}: past the end of a list of {len}"),
            Shape::Invalid { ty } => write!(f, "{path}: not a {ty}: {}", self.note.as_str()),
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
        ShapeError { path: self.at.render_in(self.alloc.clone()), shape, note: String::new_in(self.alloc.clone()) }
    }

    /// An error saying this value does not read as `ty`, for `why` (a parser's own error).
    pub fn invalid(&self, ty: &'static str, why: &dyn fmt::Display) -> ShapeError<A> {
        let mut error = self.error(Shape::Invalid { ty });
        if fmt::write(&mut error.note, format_args!("{why}")).is_err() {
            unreachable!("writing into a `String` does not fail");
        }
        error
    }

    /// This map read as a struct, strictly: `read` takes each field from the [`Fields`], and
    /// then every key it did not take is refused, all named, with the path and the known names
    /// (`a.b: unknown field "colour" (did you mean "color"?); known: "color", "size"`). The
    /// check runs here, after `read`, so no reader can leave it out.
    pub fn fields<T, E: From<ShapeError<A>>>(
        &self,
        read: impl FnOnce(&Fields<'_, 'v, 'p, A>) -> Result<T, E>,
    ) -> Result<T, E> {
        let fields = Fields::new(self)?;
        let out = read(&fields)?;
        match fields.extra().len() {
            0 => Ok(out),
            _ => Err(fields.unknown().into()),
        }
    }

    /// [`fields`](Self::fields) with the strict check opted out of, visibly: the keys `read`
    /// did not take come back beside the value, in insertion order, for the caller to look at
    /// or name as discarded.
    pub fn fields_allowing_extra<T, E: From<ShapeError<A>>>(
        &self,
        read: impl FnOnce(&Fields<'_, 'v, 'p, A>) -> Result<T, E>,
    ) -> Result<(T, Extra<'v, A>), E> {
        let fields = Fields::new(self)?;
        let out = read(&fields)?;
        Ok((out, Extra { keys: fields.extra() }))
    }

    /// This text as one of `known`, by its index; other text is refused naming it, the closest
    /// known name when one is near, and the known ones.
    pub fn one_of(&self, known: &[&str]) -> Result<usize, ShapeError<A>> {
        let text = self.text()?;
        if let Some(i) = known.iter().position(|k| *k == text) {
            return Ok(i);
        }
        let mut error = self.error(Shape::UnknownVariant);
        note_unknown(&mut error.note, "variant", &[text], known.iter().copied());
        Err(error)
    }

    /// A float, or an integer the float holds exactly: the one place an integer is read as a
    /// float, asked for by name (a `f64` field is strict).
    pub fn f64_or_int(&self) -> Result<f64, ShapeError<A>> {
        match self.value {
            Value::Float(f) => Ok(*f),
            Value::Int(n) => n.as_f64_exact().ok_or_else(|| self.error(Shape::OutOfRange { ty: "f64" })),
            _ => Err(self.expected(Kind::Float)),
        }
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

    /// The field `key` of this map, which must be there: a lookup in a map read as a
    /// dictionary. A struct is read with [`fields`](Self::fields), which refuses the keys it
    /// did not take.
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
    #[deprecated(note = "read a struct with `Cursor::fields`, which refuses the keys it did not take")]
    pub fn req<T: FromValue<A>>(&self, key: &str) -> Result<T, ShapeError<A>> {
        self.field(key)?.get()
    }

    /// The field `key` read as a `T`; `None` when it is missing or null.
    #[deprecated(note = "read a struct with `Cursor::fields`, which refuses the keys it did not take")]
    pub fn opt<T: FromValue<A>>(&self, key: &str) -> Result<Option<T>, ShapeError<A>> {
        match self.field_opt(key)? {
            Some(field) if !field.is_null() => field.get().map(Some),
            _ => Ok(None),
        }
    }

    /// Refuses a map with keys outside `known`, naming them all as
    /// [`fields`](Self::fields) does.
    #[deprecated(note = "read a struct with `Cursor::fields`, whose check cannot be left out")]
    pub fn deny_unknown(&self, known: &[&str]) -> Result<(), ShapeError<A>> {
        let map = self.map()?;
        let mut extra = blaze_own::Vec::new_in(self.alloc.clone());
        for (key, _) in map.iter() {
            if !known.contains(&key) {
                extra.push(key);
            }
        }
        match extra.is_empty() {
            true => Ok(()),
            false => {
                let mut error = self.error(Shape::UnknownFields { count: extra.len() });
                note_unknown(&mut error.note, "field", extra.as_slice(), known.iter().copied());
                Err(error)
            }
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

/// The path and the value: `Cursor(a.b[3]: true)`.
impl<A: Allocator + Clone> fmt::Debug for Cursor<'_, '_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Cursor(")?;
        self.at.write_into(f)?;
        write!(f, ": {:?})", self.value)
    }
}

/// A map being read as a struct, inside [`Cursor::fields`]: each field is taken by name, and
/// what is not taken is refused when the read ends. The names are `'static`, as a struct's
/// field names are.
pub struct Fields<'c, 'v, 'p, A: Allocator + Clone> {
    cursor: &'c Cursor<'v, 'p, A>,
    map: &'v Map<A>,
    /// The names asked for, taken or not, in the order asked: the known names an error lists.
    asked: RefCell<Vec<&'static str, A>>,
}

impl<'c, 'v, 'p, A: Allocator + Clone> Fields<'c, 'v, 'p, A> {
    fn new(cursor: &'c Cursor<'v, 'p, A>) -> Result<Fields<'c, 'v, 'p, A>, ShapeError<A>> {
        Ok(Fields { map: cursor.map()?, asked: RefCell::new(Vec::new_in(cursor.alloc.clone())), cursor })
    }

    fn ask(&self, key: &'static str) {
        let mut asked = self.asked.borrow_mut();
        if !asked.contains(&key) {
            asked.push(key);
        }
    }

    /// The field `key`, which must be there; when it is not and a key near it is, the error
    /// says so (`a.color: missing (found unknown field "colour")`), since the unknown-field
    /// check that would name it comes only after the read.
    pub fn cursor(&self, key: &'static str) -> Result<Cursor<'v, '_, A>, ShapeError<A>> {
        self.ask(key);
        self.cursor.field(key).map_err(|mut error| {
            let near = self.map.iter().map(|(k, _)| k).find(|k| closest(k, core::iter::once(key)).is_some());
            if let Some(near) = near {
                if write!(error.note, "found unknown field {near:?}").is_err() {
                    unreachable!("writing into a `String` does not fail");
                }
            }
            error
        })
    }

    /// The field `key`, if it is there.
    pub fn cursor_opt(&self, key: &'static str) -> Result<Option<Cursor<'v, '_, A>>, ShapeError<A>> {
        self.ask(key);
        self.cursor.field_opt(key)
    }

    /// The field `key` read as a `T`; missing is an error naming its path.
    pub fn take<T: FromValue<A>>(&self, key: &'static str) -> Result<T, ShapeError<A>> {
        self.cursor(key)?.get()
    }

    /// The field `key` read as a `T`; `None` when it is missing or null.
    pub fn take_opt<T: FromValue<A>>(&self, key: &'static str) -> Result<Option<T>, ShapeError<A>> {
        match self.cursor_opt(key)? {
            Some(field) if !field.is_null() => field.get().map(Some),
            _ => Ok(None),
        }
    }

    /// The keys not taken, in insertion order.
    fn extra(&self) -> Vec<&'v str, A> {
        let asked = self.asked.borrow();
        let mut extra = Vec::new_in(self.cursor.alloc.clone());
        for (key, _) in self.map.iter() {
            if !asked.iter().any(|name| *name == key) {
                extra.push(key);
            }
        }
        extra
    }

    fn unknown(&self) -> ShapeError<A> {
        let extra = self.extra();
        let mut error = self.cursor.error(Shape::UnknownFields { count: extra.len() });
        let asked = self.asked.borrow();
        note_unknown(&mut error.note, "field", extra.as_slice(), asked.iter().copied());
        error
    }
}

/// The keys a struct's reader did not take, from [`Cursor::fields_allowing_extra`].
#[must_use = "the keys the reader did not take: look at them, or name them as discarded"]
pub struct Extra<'v, A: Allocator> {
    pub keys: Vec<&'v str, A>,
}

/// `unknown field "colour" (did you mean "color"?), "x"; known: "color", "size"` into `out`.
fn note_unknown<'k, A: Allocator>(
    out: &mut String<A>,
    what: &str,
    names: &[&str],
    known: impl Iterator<Item = &'k str> + Clone,
) {
    if write_unknown(out, what, names, known).is_err() {
        unreachable!("writing into a `String` does not fail");
    }
}

fn write_unknown<'k, A: Allocator>(
    out: &mut String<A>,
    what: &str,
    names: &[&str],
    known: impl Iterator<Item = &'k str> + Clone,
) -> fmt::Result {
    let plural = if names.len() == 1 { "" } else { "s" };
    write!(out, "unknown {what}{plural} ")?;
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{name:?}")?;
        if let Some(near) = closest(name, known.clone()) {
            write!(out, " (did you mean {near:?}?)")?;
        }
    }
    out.write_str("; known: ")?;
    let mut any = false;
    for (i, name) in known.enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{name:?}")?;
        any = true;
    }
    if !any {
        out.write_str("none")?;
    }
    Ok(())
}

/// The known name nearest `name` when it is near: at most two edits away, and fewer edits than
/// `name` has bytes; the first of the nearest on a tie.
fn closest<'k>(name: &str, known: impl Iterator<Item = &'k str>) -> Option<&'k str> {
    let mut best: Option<(usize, &'k str)> = None;
    for candidate in known {
        let Some(d) = distance(name.as_bytes(), candidate.as_bytes()) else { continue };
        if d <= 2 && d < name.len() && best.is_none_or(|(b, _)| d < b) {
            best = Some((d, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// The most bytes [`distance`] compares; a longer name gets no suggestion.
const MOST_NAME: usize = 64;

/// The edit distance (insertions, deletions, substitutions) between two names of at most
/// [`MOST_NAME`] bytes, in two rows on the stack.
fn distance(a: &[u8], b: &[u8]) -> Option<usize> {
    if a.len() > MOST_NAME || b.len() > MOST_NAME {
        return None;
    }
    let mut prev = [0usize; MOST_NAME + 1];
    let mut cur = [0usize; MOST_NAME + 1];
    for (j, slot) in prev.iter_mut().enumerate().take(b.len() + 1) {
        *slot = j;
    }
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    Some(prev[b.len()])
}
