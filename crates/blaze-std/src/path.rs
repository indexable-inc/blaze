//! Relative paths (docs/v2/plan/design/std.md, "Paths are relative"): a [`Path`] cannot hold
//! a root or `..`, so every open is beneath the [`crate::fs::Dir`] it is joined to. There is no
//! current directory. A path is refused as it is made, with a [`PathError`] naming it and why.

#[cfg(test)]
use alloc::{borrow::ToOwned, string::String};
use alloc::alloc::Global;
use core::fmt;

use blaze_core::{FileName, StaticName};

use crate::{
    mem::{self, OutOfMemory},
    name::Excerpt,
};

/// A relative path: `/`-separated parts, each non-empty and none `.` or `..`, with no root, no
/// `\` and no NUL.
#[derive(Clone)] // clone: a refcount or a pointer, no allocation
pub struct Path(Text);

/// A path's text: a literal's ([`crate::path!`]), checked as its crate compiled and kept as
/// it is, or one made at run time, held as blaze-core's name of it. The literal is behind
/// one pointer so that a `Path` stays two words, as a name is.
#[derive(Clone)] // clone: a refcount or a pointer, no allocation
enum Text {
    Literal(&'static Literal),
    Name(FileName),
}

const _: () = assert!(size_of::<Path>() == size_of::<FileName>());

/// A literal [`Path::literal`] checked: the one way to [`Path::of_literal`].
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Literal(StaticName);

// Equality, order and hash are the text's, whichever way the path holds it.

impl PartialEq for Path {
    fn eq(&self, other: &Path) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Path {}

impl core::hash::Hash for Path {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for Path {
    fn partial_cmp(&self, other: &Path) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Path {
    fn cmp(&self, other: &Path) -> core::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

/// What [`Path::new`] refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct PathError {
    /// The text refused, or its start when it is long: kept in the error, which allocates
    /// nothing. For a [`Path::join`], the path joined to.
    pub path: Excerpt,
    pub why: PathWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathWhy {
    Empty,
    /// It starts at a root (`/a`) or names a drive (`C:`).
    Rooted,
    /// A part is `..`: it would leave the directory it is joined to.
    Parent,
    /// A part is `.`, or empty (`a//b`, `a/`): one path has one spelling.
    NotCanonical,
    /// A `\` (Windows' other separator) or a NUL.
    Forbidden,
    /// It passed bstd's rule and blaze-core's `FileName` refused it: the two rules disagree,
    /// which is a bug of one of them.
    NoName,
    /// There was no room for the joined text ([`Path::join`]).
    OutOfMemory(OutOfMemory),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "path {:?} {}", self.path, self.why.says())?;
        match self.why {
            PathWhy::OutOfMemory(error) => write!(f, " ({error})"),
            _ => Ok(()),
        }
    }
}

impl core::error::Error for PathError {}

impl PathWhy {
    pub(crate) const fn says(self) -> &'static str {
        match self {
            PathWhy::Empty => "is empty",
            PathWhy::Rooted => "starts at a root: a bstd path is relative to its directory",
            PathWhy::Parent => "has a `..`: a bstd path never leaves its directory",
            PathWhy::NotCanonical => "has a `.` or an empty part: write it as one relative path",
            PathWhy::Forbidden => "has a `\\` or a NUL",
            PathWhy::NoName => {
                "passed bstd's path rule and is no blaze-core file name (the two rules disagree: a bug)"
            }
            PathWhy::OutOfMemory(_) => "could not be joined: the global heap had no room",
        }
    }
}

/// Why `raw` is no path, if it is not. `parent` lets `..` through, for the test that plants it.
/// A `const fn`, so [`crate::path!`] checks a literal as the crate compiles by the same rule
/// [`Path::new`] checks at run time. It walks the bytes by slice patterns, so no index can
/// fall outside them; each turn of each loop takes at least one byte.
const fn refused(raw: &str, parent: bool) -> Option<PathWhy> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Some(PathWhy::Empty);
    }
    if let [b'/', ..] | [_, b':', ..] = bytes {
        return Some(PathWhy::Rooted);
    }
    let mut rest = bytes;
    while let [byte, tail @ ..] = rest {
        if *byte == b'\\' || *byte == 0 {
            return Some(PathWhy::Forbidden);
        }
        rest = tail;
    }
    // Each `/`-separated part: `rest` starts with it, and `len` bytes long it ends where
    // `scan` starts, at a separator or the end.
    let mut rest = bytes;
    loop {
        let mut len = 0;
        let mut scan = rest;
        while let [byte, tail @ ..] = scan {
            if *byte == b'/' {
                break;
            }
            len += 1;
            scan = tail;
        }
        if len == 2 && matches!(rest, [b'.', b'.', ..]) {
            if !parent {
                return Some(PathWhy::Parent);
            }
        } else if len == 0 || (len == 1 && matches!(rest, [b'.', ..])) {
            return Some(PathWhy::NotCanonical);
        }
        match scan {
            // Past the separator: the next part, empty when the path ends with one.
            [_, tail @ ..] => rest = tail,
            [] => return None,
        }
    }
}

/// A [`Path`] from a string literal, checked as the crate compiles: `path!("data/log")`. A bad
/// literal (`path!("../x")`) is a compile error saying why. It is a [`Path`], not a `Result`:
/// the check is done, and the path keeps the literal itself, so it allocates nothing and
/// nothing is left that could fail.
#[macro_export]
macro_rules! path {
    ($raw:literal) => {{
        const LITERAL: $crate::path::Literal = match $crate::path::Path::literal($raw) {
            Ok(literal) => literal,
            Err(why) => panic!("{}", why),
        };
        const PATH: $crate::path::Path = $crate::path::Path::of_literal(&LITERAL);
        PATH
    }};
}

/// The text `write` makes, as a name under the driver's root: `Ok(None)` when it is none.
/// The text is made on the global heap and dropped once blaze-core's `FileName` has its own
/// copy; growing it is the one failure, [`OutOfMemory`].
pub(crate) fn name_of(
    write: impl FnOnce(&mut mem::String<Global>) -> Result<(), OutOfMemory>,
) -> Result<Option<FileName>, OutOfMemory> {
    // alloc: once per name made, its text, until blaze-core's `FileName` copies it.
    let mut text = mem::String::new_in(Global);
    write(&mut text)?;
    // alloc: blaze-core's `FileName` copies the text into its own `Arc<str>`, which it makes
    // infallibly (blaze-core is not ported to fallible allocation).
    Ok(FileName::new(text.as_str()))
}

/// `n` in decimal at the end of `text`.
pub(crate) fn push_decimal(text: &mut mem::String<Global>, n: u64) -> Result<(), OutOfMemory> {
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut rest = n;
    for slot in &mut digits {
        *slot = b'0' + (rest % 10) as u8;
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    for digit in digits.iter().take(count).rev() {
        text.push(char::from(*digit))?;
    }
    Ok(())
}

impl Path {
    pub fn new(raw: &str) -> Result<Path, PathError> {
        let refuse = |why| PathError { path: Excerpt::of(raw), why };
        if let Some(why) = refused(raw, false) {
            return Err(refuse(why));
        }
        // alloc: once per path, blaze-core's `FileName`, which it makes infallibly
        // (blaze-core is not ported to fallible allocation).
        match FileName::new(raw) {
            Some(name) => Ok(Path(Text::Name(name))),
            None => Err(refuse(PathWhy::NoName)),
        }
    }

    /// [`Path::new`]'s check of a literal, what [`crate::path!`] evaluates in a `const`: the
    /// literal, checked by this rule and by blaze-core's rule for a name, or why it is no
    /// path. The macro turns a refusal into a compile error saying it.
    #[doc(hidden)]
    pub const fn literal(raw: &'static str) -> Result<Literal, &'static str> {
        if let Some(why) = refused(raw, false) {
            return Err(why.says());
        }
        match FileName::checked(raw) {
            Some(name) => Ok(Literal(name)),
            None => Err(PathWhy::NoName.says()),
        }
    }

    /// The path of a checked literal: it keeps the literal itself, so it allocates nothing.
    #[doc(hidden)]
    pub const fn of_literal(literal: &'static Literal) -> Path {
        Path(Text::Literal(literal))
    }

    /// `raw` with `..` let through, for the test that plants [`crate::Plant::LexicalParent`]:
    /// the join must then resolve it by text, which is the bug.
    #[cfg(test)]
    pub(crate) fn with_parents(raw: &str) -> Result<String, PathError> {
        match refused(raw, true) {
            Some(why) => Err(PathError { path: Excerpt::of(raw), why }),
            // alloc: a test's planted path.
            None => Ok(raw.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Text::Literal(literal) => literal.0.as_str(),
            Text::Name(name) => name.as_str(),
        }
    }

    /// `self` then `more`: two paths beneath a directory make one beneath it. With no room
    /// for the joined text it is [`PathWhy::OutOfMemory`], naming the two.
    pub fn join(&self, more: &Path) -> Result<Path, PathError> {
        let (head, tail) = (self.as_str(), more.as_str());
        let refuse = |why| PathError { path: Excerpt::of_parts(&[head, "/", tail]), why };
        let joined = name_of(|text| {
            text.reserve(head.len() + 1 + tail.len())?;
            text.push_str(head)?;
            text.push('/')?;
            text.push_str(tail)
        })
        .map_err(|error| refuse(PathWhy::OutOfMemory(error)))?;
        joined.map(Path::of).ok_or_else(|| refuse(PathWhy::NoName))
    }

    /// The path as blaze-core's name of it, for an effect: a refcount of the one it holds,
    /// or, for a literal, the name made of it now (its check was done as the crate compiled).
    pub(crate) fn to_name(&self) -> FileName {
        match &self.0 {
            // alloc: once per use of a literal path as a name, in blaze-core's `FileName`,
            // which it makes infallibly (blaze-core is not ported to fallible allocation).
            Text::Literal(literal) => FileName::of_static(literal.0),
            // clone: a refcount.
            Text::Name(name) => name.clone(),
        }
    }

    pub(crate) fn into_name(self) -> FileName {
        match self.0 {
            Text::Literal(_) => self.to_name(),
            Text::Name(name) => name,
        }
    }

    pub(crate) fn of(name: FileName) -> Path {
        Path(Text::Name(name))
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Path({:?})", self.as_str())
    }
}

impl TryFrom<&str> for Path {
    type Error = PathError;

    fn try_from(raw: &str) -> Result<Path, PathError> {
        Path::new(raw)
    }
}
