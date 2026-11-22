//! [`Ident`]: a version's pre-release or build metadata as written, up to [`INLINE`] bytes in
//! place and longer in the caller's allocator; [`Prerelease`] and [`BuildMetadata`] order it as
//! SemVer and the `semver` crate do.

use core::{
    alloc::Allocator,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
};

use blaze_own::{
    String,
    planted::{NoPlant, Planted},
};

use crate::SemverPlant;

/// The most bytes an identifier keeps in place. Cargo's pre-releases and build metadata are
/// almost all shorter (`alpha.1`, `rc.2`, `20260101`), so a version allocates only past this.
pub const INLINE: usize = 23;

/// Dotted identifiers as written, validated by the parse that made them; empty means none.
pub struct Ident<A: Allocator> {
    repr: Repr<A>,
}

enum Repr<A: Allocator> {
    Inline { len: u8, bytes: [u8; INLINE] },
    Heap(String<A>),
}

impl<A: Allocator> Ident<A> {
    /// No identifiers; allocates nothing.
    pub const EMPTY: Ident<A> = Ident { repr: Repr::Inline { len: 0, bytes: [0; INLINE] } };

    /// `text`, already validated: in place when it fits, else copied into `alloc`.
    pub(crate) fn from_valid(text: &str, alloc: A) -> Ident<A> {
        match u8::try_from(text.len()) {
            Ok(len) if text.len() <= INLINE => {
                let mut bytes = [0; INLINE];
                bytes[..text.len()].copy_from_slice(text.as_bytes());
                Ident { repr: Repr::Inline { len, bytes } }
            }
            _ => Ident { repr: Repr::Heap(String::from_str_in(text, alloc)) },
        }
    }

    /// The identifiers as written, dots included.
    pub fn as_str(&self) -> &str {
        match &self.repr {
            Repr::Inline { len, bytes } => match core::str::from_utf8(&bytes[..usize::from(*len)]) {
                Ok(text) => text,
                Err(error) => unreachable!("an inline identifier is copied from a `str`: {error}"),
            },
            Repr::Heap(text) => text.as_str(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }

    /// Whether the text lives in the allocator, not in place.
    pub fn is_heap(&self) -> bool {
        matches!(self.repr, Repr::Heap(_))
    }
}

impl<A: Allocator + Clone> Clone for Ident<A> {
    fn clone(&self) -> Ident<A> {
        match &self.repr {
            Repr::Inline { len, bytes } => Ident { repr: Repr::Inline { len: *len, bytes: *bytes } },
            Repr::Heap(text) => Ident { repr: Repr::Heap(text.clone()) },
        }
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Ident<B>> for Ident<A> {
    fn eq(&self, other: &Ident<B>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<A: Allocator> Eq for Ident<A> {}

impl<A: Allocator> Hash for Ident<A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl<A: Allocator> fmt::Display for Ident<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<A: Allocator> fmt::Debug for Ident<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// One identifier against another by the rule both pre-releases and build metadata use:
/// numeric ones by value (compared as digit strings, so any length works), numeric before
/// alphanumeric, alphanumeric ones by ASCII; with leading zeros (build metadata only) the
/// values tie and the shorter writing comes first, so distinct texts never order equal.
fn cmp_identifier(a: &str, b: &str) -> Ordering {
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    match (numeric(a), numeric(b)) {
        (true, true) => {
            let (ta, tb) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
            ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| a.len().cmp(&b.len()))
        }
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.cmp(b),
    }
}

/// Dotted identifiers in order, a shorter list first when it is a prefix of the other (SemVer
/// 2.0.0, section 11.4.4). Under [`SemverPlant::NumbersAsText`] each pair compares as text.
fn cmp_dotted(a: &str, b: &str, plant: impl Planted<SemverPlant>) -> Ordering {
    let one: fn(&str, &str) -> Ordering = match plant.has(SemverPlant::NumbersAsText) {
        true => <str as Ord>::cmp,
        false => cmp_identifier,
    };
    let (mut xs, mut ys) = (a.split('.'), b.split('.'));
    loop {
        match (xs.next(), ys.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match one(x, y) {
                Ordering::Equal => {}
                other => return other,
            },
        }
    }
}

macro_rules! dotted {
    ($(#[$meta:meta])* $name:ident, $empty_first:expr) => {
        $(#[$meta])*
        pub struct $name<A: Allocator>(pub Ident<A>);

        impl<A: Allocator> $name<A> {
            /// None; allocates nothing.
            pub const EMPTY: $name<A> = $name(Ident::EMPTY);

            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }

            /// `Ord`'s order with a planted bug from `plant`: the seam the precedence oracle's
            /// control drives. `Ord` passes `NoPlant`, and the order is the code as it is.
            pub fn cmp_planted(&self, other: &$name<A>, plant: impl Planted<SemverPlant>) -> Ordering {
                match (self.is_empty(), other.is_empty()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => if $empty_first { Ordering::Less } else { Ordering::Greater },
                    (false, true) => if $empty_first { Ordering::Greater } else { Ordering::Less },
                    (false, false) => cmp_dotted(self.as_str(), other.as_str(), plant),
                }
            }
        }

        impl<A: Allocator + Clone> Clone for $name<A> {
            fn clone(&self) -> $name<A> {
                $name(self.0.clone())
            }
        }

        impl<A: Allocator, B: Allocator> PartialEq<$name<B>> for $name<A> {
            fn eq(&self, other: &$name<B>) -> bool {
                self.0 == other.0
            }
        }

        impl<A: Allocator> Eq for $name<A> {}

        impl<A: Allocator> Hash for $name<A> {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.0.hash(state);
            }
        }

        impl<A: Allocator> PartialOrd for $name<A> {
            fn partial_cmp(&self, other: &$name<A>) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        impl<A: Allocator> Ord for $name<A> {
            fn cmp(&self, other: &$name<A>) -> Ordering {
                self.cmp_planted(other, NoPlant)
            }
        }

        impl<A: Allocator> fmt::Display for $name<A> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl<A: Allocator> fmt::Debug for $name<A> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(self.as_str(), f)
            }
        }
    };
}

dotted!(
    /// A pre-release (`alpha.1`). The empty one orders after every other: `1.0.0-rc.1 < 1.0.0`.
    Prerelease,
    false
);

dotted!(
    /// Build metadata (`20260101.sha.5114f85`). Precedence ignores it; `Version`'s `Ord` puts
    /// the empty one first, then compares as a pre-release's identifiers.
    BuildMetadata,
    true
);

impl<A: Allocator> Prerelease<A> {
    /// `text` as a pre-release, checked as a version's would be, in `alloc` past [`INLINE`]
    /// bytes. The empty text is [`Prerelease::EMPTY`].
    pub fn new_in(text: &str, alloc: A) -> Result<Prerelease<A>, crate::Error> {
        if text.is_empty() {
            return Ok(Prerelease::EMPTY);
        }
        let mut cursor = crate::Cursor::new(text, crate::Limits::CARGO)?;
        cursor.identifiers(crate::Part::Pre)?;
        match cursor.at == text.len() {
            true => Ok(Prerelease(Ident::from_valid(text, alloc))),
            false => Err(cursor.unexpected(crate::Part::Pre)),
        }
    }
}

impl<A: Allocator> BuildMetadata<A> {
    /// `text` as build metadata, checked as a version's would be, in `alloc` past [`INLINE`]
    /// bytes. The empty text is [`BuildMetadata::EMPTY`].
    pub fn new_in(text: &str, alloc: A) -> Result<BuildMetadata<A>, crate::Error> {
        if text.is_empty() {
            return Ok(BuildMetadata::EMPTY);
        }
        let mut cursor = crate::Cursor::new(text, crate::Limits::CARGO)?;
        cursor.identifiers(crate::Part::Build)?;
        match cursor.at == text.len() {
            true => Ok(BuildMetadata(Ident::from_valid(text, alloc))),
            false => Err(cursor.unexpected(crate::Part::Build)),
        }
    }
}

/// The order of two pre-releases' texts, both non-empty and validated, whatever allocators
/// hold them.
pub(crate) fn cmp_pre_text(a: &str, b: &str) -> Ordering {
    cmp_dotted(a, b, NoPlant)
}
