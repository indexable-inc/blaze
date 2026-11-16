//! [`VersionReq`]: Cargo's version requirements and how a version matches one.
//!
//! The grammar and the matching rules are Cargo's, which are the `semver` crate's
//! (`semver` 1.0, `parse.rs` and `eval.rs`): restated here from their documented behavior, as
//! the tests' vectors from the Cargo book pin.

use core::{alloc::Allocator, fmt};

use blaze_own::Vec;

use crate::{Cursor, Error, Ident, Limits, Part, Prerelease, Version, What};

/// A comparator's operator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Op {
    /// `=I.J.K`: exactly; `=I.J` and `=I` as wildcards.
    Exact,
    /// `>I.J.K`.
    Greater,
    /// `>=I.J.K`.
    GreaterEq,
    /// `<I.J.K`.
    Less,
    /// `<=I.J.K`.
    LessEq,
    /// `~I.J.K`: the patch may grow (`>=I.J.K, <I.(J+1).0`); `~I` lets the minor grow too.
    Tilde,
    /// `^I.J.K` and a bare `I.J.K`: what the leftmost non-zero number allows.
    Caret,
    /// `I.*` and `I.J.*`.
    Wildcard,
}

/// One comparator: an operator and a version that may leave out its minor and patch.
pub struct Comparator<A: Allocator> {
    pub op: Op,
    pub major: u64,
    pub minor: Option<u64>,
    /// `None` whenever `minor` is.
    pub patch: Option<u64>,
    /// Empty unless `patch` is given.
    pub pre: Prerelease<A>,
}

/// Comparators all of which a version must match; none is `*`, which every release matches.
pub struct VersionReq<A: Allocator> {
    pub comparators: Vec<Comparator<A>, A>,
}

impl<A: Allocator> VersionReq<A> {
    /// `*`: no comparators.
    pub fn star_in(alloc: A) -> VersionReq<A> {
        VersionReq { comparators: Vec::new_in(alloc) }
    }

    /// `text` as a requirement under [`Limits::CARGO`], in `A`'s default allocator
    /// (`VersionReq::<Global>::parse("^1.2")`).
    pub fn parse(text: &str) -> Result<VersionReq<A>, Error>
    where
        A: Clone + Default,
    {
        VersionReq::parse_in(text, Limits::CARGO, A::default())
    }

    /// `text` as a requirement: comparators separated by `,`, blanks allowed around each.
    /// A pre-release past [`INLINE`](crate::INLINE) bytes goes in `alloc`.
    pub fn parse_in(text: &str, limits: Limits, alloc: A) -> Result<VersionReq<A>, Error>
    where
        A: Clone,
    {
        let mut cursor = Cursor::new(text, limits)?;
        let mut comparators = Vec::new_in(alloc.clone());
        cursor.spaces();
        if let Some(b'*' | b'x' | b'X') = cursor.peek() {
            cursor.at += 1;
            cursor.spaces();
            return match cursor.peek() {
                None => Ok(VersionReq { comparators }),
                Some(b',') => Err(cursor.err(Part::Requirement, What::WildcardNotAlone)),
                Some(_) => Err(cursor.err(Part::Requirement, What::AfterWildcard)),
            };
        }
        loop {
            if comparators.len() == limits.most_comparators {
                let most = limits.most_comparators;
                return Err(cursor.err(Part::Requirement, What::TooManyComparators { most }));
            }
            comparators.push(comparator(&mut cursor, alloc.clone())?);
            cursor.spaces();
            match cursor.text[cursor.at..].chars().next() {
                None => return Ok(VersionReq { comparators }),
                Some(',') => {
                    cursor.at += 1;
                    cursor.spaces();
                }
                Some(ch) => return Err(cursor.err(Part::Requirement, What::ExpectedComma(ch))),
            }
        }
    }

    /// Whether `version` matches every comparator, and, when it is a pre-release, whether some
    /// comparator names its `major.minor.patch` with a pre-release (Cargo's rule: `^1.2.0`
    /// does not admit `1.3.0-alpha`, `>=1.2.0-alpha, <2` admits `1.2.0-beta` but not
    /// `1.3.0-beta`).
    pub fn matches<B: Allocator>(&self, version: &Version<B>) -> bool {
        if !self.comparators.iter().all(|c| c.matches_ignoring_pre_rule(version)) {
            return false;
        }
        version.pre.is_empty() || self.comparators.iter().any(|c| c.admits_pre_of(version))
    }

    /// Whether this is `*`.
    pub fn is_star(&self) -> bool {
        self.comparators.is_empty()
    }
}

/// One comparator from the cursor, stopping at the first byte past it.
fn comparator<A: Allocator>(cursor: &mut Cursor<'_>, alloc: A) -> Result<Comparator<A>, Error> {
    let explicit = match (cursor.peek(), cursor.text.as_bytes().get(cursor.at + 1).copied()) {
        (Some(b'>'), Some(b'=')) => Some((Op::GreaterEq, 2)),
        (Some(b'<'), Some(b'=')) => Some((Op::LessEq, 2)),
        (Some(b'='), _) => Some((Op::Exact, 1)),
        (Some(b'>'), _) => Some((Op::Greater, 1)),
        (Some(b'<'), _) => Some((Op::Less, 1)),
        (Some(b'~'), _) => Some((Op::Tilde, 1)),
        (Some(b'^'), _) => Some((Op::Caret, 1)),
        _ => None,
    };
    let mut op = match explicit {
        Some((op, len)) => {
            cursor.at += len;
            cursor.spaces();
            op
        }
        None => Op::Caret,
    };
    let bare = explicit.is_none();
    let wildcard = |cursor: &mut Cursor<'_>| match cursor.peek() {
        Some(b'*' | b'x' | b'X') => {
            cursor.at += 1;
            true
        }
        _ => false,
    };
    let major = cursor.number(Part::Major)?;
    let mut wild = false;
    let mut minor = None;
    let mut patch = None;
    if cursor.eat(b'.') {
        if wildcard(cursor) {
            wild = true;
            if bare {
                op = Op::Wildcard;
            }
        } else {
            minor = Some(cursor.number(Part::Minor)?);
        }
    }
    if cursor.eat(b'.') {
        if wildcard(cursor) {
            if bare {
                op = Op::Wildcard;
            }
            wild = true;
        } else if wild {
            return Err(cursor.err(Part::Patch, What::AfterWildcard));
        } else {
            patch = Some(cursor.number(Part::Patch)?);
        }
    }
    let mut pre = Prerelease::EMPTY;
    if patch.is_some() && cursor.eat(b'-') {
        pre = Prerelease(Ident::from_valid(cursor.identifiers(Part::Pre)?, alloc));
    }
    if patch.is_some() && cursor.eat(b'+') {
        // Build metadata in a requirement is read and has no effect, as Cargo has it.
        cursor.identifiers(Part::Build)?;
    }
    Ok(Comparator { op, major, minor, patch, pre })
}

impl<A: Allocator> Comparator<A> {
    /// Whether `version` satisfies this comparator, before the pre-release rule.
    pub fn matches_ignoring_pre_rule<B: Allocator>(&self, v: &Version<B>) -> bool {
        match self.op {
            Op::Exact | Op::Wildcard => self.exact(v),
            Op::Greater => self.greater(v),
            Op::GreaterEq => self.exact(v) || self.greater(v),
            Op::Less => self.less(v),
            Op::LessEq => self.exact(v) || self.less(v),
            Op::Tilde => self.tilde(v),
            Op::Caret => self.caret(v),
        }
    }

    /// Whether this comparator lets a pre-release of `v`'s `major.minor.patch` in.
    pub fn admits_pre_of<B: Allocator>(&self, v: &Version<B>) -> bool {
        self.major == v.major
            && self.minor == Some(v.minor)
            && self.patch == Some(v.patch)
            && !self.pre.is_empty()
    }

    fn pre_cmp<B: Allocator>(&self, v: &Version<B>) -> core::cmp::Ordering {
        // Both orders treat the empty pre-release as the greatest; compare by text across
        // allocators.
        match (v.pre.is_empty(), self.pre.is_empty()) {
            (true, true) => core::cmp::Ordering::Equal,
            (true, false) => core::cmp::Ordering::Greater,
            (false, true) => core::cmp::Ordering::Less,
            (false, false) => crate::ident::cmp_pre_text(v.pre.as_str(), self.pre.as_str()),
        }
    }

    fn exact<B: Allocator>(&self, v: &Version<B>) -> bool {
        v.major == self.major
            && self.minor.is_none_or(|m| v.minor == m)
            && self.patch.is_none_or(|p| v.patch == p)
            && v.pre.as_str() == self.pre.as_str()
    }

    fn greater<B: Allocator>(&self, v: &Version<B>) -> bool {
        if v.major != self.major {
            return v.major > self.major;
        }
        let Some(minor) = self.minor else { return false };
        if v.minor != minor {
            return v.minor > minor;
        }
        let Some(patch) = self.patch else { return false };
        if v.patch != patch {
            return v.patch > patch;
        }
        self.pre_cmp(v).is_gt()
    }

    fn less<B: Allocator>(&self, v: &Version<B>) -> bool {
        if v.major != self.major {
            return v.major < self.major;
        }
        let Some(minor) = self.minor else { return false };
        if v.minor != minor {
            return v.minor < minor;
        }
        let Some(patch) = self.patch else { return false };
        if v.patch != patch {
            return v.patch < patch;
        }
        self.pre_cmp(v).is_lt()
    }

    fn tilde<B: Allocator>(&self, v: &Version<B>) -> bool {
        if v.major != self.major {
            return false;
        }
        if let Some(minor) = self.minor
            && v.minor != minor
        {
            return false;
        }
        if let Some(patch) = self.patch
            && v.patch != patch
        {
            return v.patch > patch;
        }
        self.pre_cmp(v).is_ge()
    }

    fn caret<B: Allocator>(&self, v: &Version<B>) -> bool {
        if v.major != self.major {
            return false;
        }
        let Some(minor) = self.minor else { return true };
        let Some(patch) = self.patch else {
            return match self.major > 0 {
                true => v.minor >= minor,
                false => v.minor == minor,
            };
        };
        if self.major > 0 {
            if v.minor != minor {
                return v.minor > minor;
            }
            if v.patch != patch {
                return v.patch > patch;
            }
        } else if minor > 0 {
            if v.minor != minor {
                return false;
            }
            if v.patch != patch {
                return v.patch > patch;
            }
        } else if v.minor != minor || v.patch != patch {
            return false;
        }
        self.pre_cmp(v).is_ge()
    }
}

impl<A: Allocator + Clone> Clone for Comparator<A> {
    fn clone(&self) -> Comparator<A> {
        Comparator {
            op: self.op,
            major: self.major,
            minor: self.minor,
            patch: self.patch,
            pre: self.pre.clone(),
        }
    }
}

impl<A: Allocator + Clone> Clone for VersionReq<A> {
    fn clone(&self) -> VersionReq<A> {
        VersionReq { comparators: self.comparators.clone() }
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Comparator<B>> for Comparator<A> {
    fn eq(&self, other: &Comparator<B>) -> bool {
        (self.op, self.major, self.minor, self.patch) == (other.op, other.major, other.minor, other.patch)
            && self.pre == other.pre
    }
}

impl<A: Allocator> Eq for Comparator<A> {}

impl<A: Allocator> core::hash::Hash for Comparator<A> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        (self.op, self.major, self.minor, self.patch).hash(state);
        self.pre.hash(state);
    }
}

impl<A: Allocator, B: Allocator> PartialEq<VersionReq<B>> for VersionReq<A> {
    fn eq(&self, other: &VersionReq<B>) -> bool {
        self.comparators.len() == other.comparators.len()
            && self.comparators.iter().zip(other.comparators.iter()).all(|(a, b)| a == b)
    }
}

impl<A: Allocator> Eq for VersionReq<A> {}

impl<A: Allocator> core::hash::Hash for VersionReq<A> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.comparators.len().hash(state);
        for c in self.comparators.iter() {
            c.hash(state);
        }
    }
}

impl<A: Allocator> fmt::Display for Comparator<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.op {
            Op::Exact => "=",
            Op::Greater => ">",
            Op::GreaterEq => ">=",
            Op::Less => "<",
            Op::LessEq => "<=",
            Op::Tilde => "~",
            Op::Caret => "^",
            Op::Wildcard => "",
        })?;
        write!(f, "{}", self.major)?;
        match (self.minor, self.patch) {
            (Some(minor), Some(patch)) => {
                write!(f, ".{minor}.{patch}")?;
                if !self.pre.is_empty() {
                    write!(f, "-{}", self.pre)?;
                }
            }
            (Some(minor), None) => {
                write!(f, ".{minor}")?;
                if self.op == Op::Wildcard {
                    f.write_str(".*")?;
                }
            }
            (None, _) if self.op == Op::Wildcard => f.write_str(".*")?,
            (None, _) => {}
        }
        Ok(())
    }
}

impl<A: Allocator> fmt::Debug for Comparator<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Comparator({self})")
    }
}

/// `*` when there are no comparators, else each joined by `, `: text that parses back to an
/// equal requirement.
impl<A: Allocator> fmt::Display for VersionReq<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.comparators.is_empty() {
            return f.write_str("*");
        }
        for (i, c) in self.comparators.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl<A: Allocator> fmt::Debug for VersionReq<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VersionReq({self})")
    }
}
