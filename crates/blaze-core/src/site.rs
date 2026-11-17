//! [`Site`]: where blaze's own code was when it did something a value remembers (made an
//! error, took a lock, opened a frame, owed a write), the one type for it: a return trace's
//! entry, a lock's holder, a watchdog's stall line.
//!
//! The rule (docs/lints.md, `absolute_position`): a position that reaches code or a kept value
//! is item-relative, and the absolute `file:line:column` is made only where it is printed. In
//! code blaze builds, a `core::panic::Location` is a record the link places for one span of one
//! item (`blaze_codegen::Target::Position`, a `blaze_format::SpanRef`: the item's anchor and the
//! span's index in its table), and a line added above the function rewrites that record's line
//! in place, in the image and in a running program's live patch, compiling nothing. A `Site` is
//! that record by reference, so it is item-relative as the code is: it reads the line the link
//! wrote last, never the line it was made at. A copy of the record (`*Location::caller()`) or
//! of its line (`.line()`) kept in a value is absolute: it goes stale under the live patch and,
//! hashed or stored, it moves with every edit above it.

use std::{fmt, panic::Location};

/// Where code was, as the link placed it: the `core::panic::Location` record of one span of
/// one item, by reference ([the module](self)).
///
/// The invariant: a `Site` holds no position, only which record. It is rendered (`Display`,
/// `file:line:column`) where it is printed and nowhere else, and is neither `Hash`, `Ord` nor
/// serializable: by its address those would differ between runs (deterministic-output), by its
/// line they would move with every edit above the code (keys-cover-every-input). A value that
/// outlives the process holds what a `Site` renders to, as text it shows, never as a key.
#[derive(Clone, Copy)]
pub struct Site(&'static Location<'static>);

impl Site {
    /// The site of the caller: through every `#[track_caller]` function between, as
    /// [`Location::caller`].
    #[track_caller]
    #[inline]
    pub const fn here() -> Site {
        Site(Location::caller())
    }

    /// The site `location` is the record of.
    #[inline]
    pub const fn of(location: &'static Location<'static>) -> Site {
        Site(location)
    }

    /// The record, read now: for a renderer, which reads its file, line and column as the
    /// link last wrote them.
    #[inline]
    pub const fn location(self) -> &'static Location<'static> {
        self.0
    }
}

/// One record, one site: by address, within the process (two records of one position are two
/// spans, and two sites).
impl PartialEq for Site {
    fn eq(&self, other: &Site) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for Site {}

impl From<&'static Location<'static>> for Site {
    fn from(location: &'static Location<'static>) -> Site {
        Site(location)
    }
}

/// `file:line:column`, rendered from the record as it is now.
impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.0, f)
    }
}

impl fmt::Debug for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Site({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Site;

    #[track_caller]
    fn caller() -> Site {
        Site::here()
    }

    /// A site renders the absolute `file:line:column` of the call it is of (through a
    /// `#[track_caller]` function), the same as its record does; two calls are two sites, and
    /// one site copied is one.
    #[test]
    fn a_site_renders_where_its_call_is_written() {
        let line = line!() + 1;
        let one = caller();
        let other = caller();
        assert_eq!(one.location().line(), line);
        assert_eq!(one.location().file(), file!());
        assert_eq!(one.to_string(), format!("{}:{line}:{}", file!(), one.location().column()));
        assert_ne!(one, other, "two calls, two records");
        let copied = one;
        assert_eq!(copied, one);
    }
}
