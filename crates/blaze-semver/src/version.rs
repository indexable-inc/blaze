//! [`Version`]: `major.minor.patch[-pre][+build]`.

use core::{
    alloc::Allocator,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
};

use blaze_own::planted::{NoPlant, Planted};

use crate::{BuildMetadata, Cursor, Error, Ident, Limits, Part, Prerelease, SemverPlant};

/// A semantic version.
pub struct Version<A: Allocator> {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Prerelease<A>,
    pub build: BuildMetadata<A>,
}

impl<A: Allocator> Version<A> {
    /// `major.minor.patch`, with no pre-release or build metadata; allocates nothing.
    pub const fn new(major: u64, minor: u64, patch: u64) -> Version<A> {
        Version { major, minor, patch, pre: Prerelease::EMPTY, build: BuildMetadata::EMPTY }
    }

    /// `text` as a version, strictly: no blanks, no `v`, no leading zeros, nothing after.
    /// A pre-release or build metadata past [`INLINE`](crate::INLINE) bytes goes in `alloc`.
    pub fn parse_in(text: &str, limits: Limits, alloc: A) -> Result<Version<A>, Error>
    where
        A: Clone,
    {
        let mut cursor = Cursor::new(text, limits)?;
        let version = parse(&mut cursor, alloc)?;
        match cursor.peek() {
            None => Ok(version),
            Some(_) => {
                let part = match (version.pre.is_empty(), version.build.is_empty()) {
                    (_, false) => Part::Build,
                    (false, true) => Part::Pre,
                    (true, true) => Part::Patch,
                };
                Err(cursor.unexpected(part))
            }
        }
    }

    /// `text` as a version under [`Limits::CARGO`], a long identifier in `A`'s default
    /// allocator (`Version::<Global>::parse("1.2.3")`).
    pub fn parse(text: &str) -> Result<Version<A>, Error>
    where
        A: Clone + Default,
    {
        Version::parse_in(text, Limits::CARGO, A::default())
    }

    /// The same version in `alloc`: its identifiers were validated when it was made, so they
    /// are copied, not parsed again (a lock's versions, in the lock's allocator, compared with
    /// ranges parsed in another).
    pub fn clone_in<B: Allocator + Clone>(&self, alloc: B) -> Version<B> {
        let pre = match self.pre.is_empty() {
            true => Prerelease::EMPTY,
            false => Prerelease(Ident::from_valid(self.pre.as_str(), alloc.clone())),
        };
        let build = match self.build.is_empty() {
            true => BuildMetadata::EMPTY,
            false => BuildMetadata(Ident::from_valid(self.build.as_str(), alloc)),
        };
        Version { major: self.major, minor: self.minor, patch: self.patch, pre, build }
    }

    /// The order of SemVer 2.0.0's section 11: by the numbers, then a pre-release before the
    /// release; build metadata does not count.
    pub fn cmp_precedence(&self, other: &Version<A>) -> Ordering {
        self.cmp_precedence_planted(other, NoPlant)
    }

    /// [`Version::cmp_precedence`] with a planted bug from `plant`: the seam the precedence
    /// oracle's control drives. Shipped callers pass `NoPlant` (through `cmp_precedence` and
    /// `Ord`), and the order is the code as it is.
    pub fn cmp_precedence_planted(&self, other: &Version<A>, plant: impl Planted<SemverPlant>) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| self.pre.cmp_planted(&other.pre, plant))
    }
}

/// `major.minor.patch[-pre][+build]` from the cursor, stopping at the first byte past it.
fn parse<A: Allocator + Clone>(cursor: &mut Cursor<'_>, alloc: A) -> Result<Version<A>, Error> {
    let major = cursor.number(Part::Major)?;
    if !cursor.eat(b'.') {
        return Err(cursor.unexpected(Part::Major));
    }
    let minor = cursor.number(Part::Minor)?;
    if !cursor.eat(b'.') {
        return Err(cursor.unexpected(Part::Minor));
    }
    let patch = cursor.number(Part::Patch)?;
    let pre = match cursor.eat(b'-') {
        true => Prerelease(Ident::from_valid(cursor.identifiers(Part::Pre)?, alloc.clone())),
        false => Prerelease::EMPTY,
    };
    let build = match cursor.eat(b'+') {
        true => BuildMetadata(Ident::from_valid(cursor.identifiers(Part::Build)?, alloc)),
        false => BuildMetadata::EMPTY,
    };
    Ok(Version { major, minor, patch, pre, build })
}

impl<A: Allocator + Clone> Clone for Version<A> {
    fn clone(&self) -> Version<A> {
        Version {
            major: self.major,
            minor: self.minor,
            patch: self.patch,
            pre: self.pre.clone(),
            build: self.build.clone(),
        }
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Version<B>> for Version<A> {
    fn eq(&self, other: &Version<B>) -> bool {
        (self.major, self.minor, self.patch) == (other.major, other.minor, other.patch)
            && self.pre == other.pre
            && self.build == other.build
    }
}

impl<A: Allocator> Eq for Version<A> {}

impl<A: Allocator> Hash for Version<A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.major, self.minor, self.patch).hash(state);
        self.pre.hash(state);
        self.build.hash(state);
    }
}

impl<A: Allocator> PartialOrd for Version<A> {
    fn partial_cmp(&self, other: &Version<A>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Precedence, then build metadata, so `Ord` agrees with `Eq` (as the `semver` crate's does).
impl<A: Allocator> Ord for Version<A> {
    fn cmp(&self, other: &Version<A>) -> Ordering {
        self.cmp_precedence(other).then_with(|| self.build.cmp(&other.build))
    }
}

impl<A: Allocator> fmt::Display for Version<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre)?;
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build)?;
        }
        Ok(())
    }
}

impl<A: Allocator> fmt::Debug for Version<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Version({self})")
    }
}
