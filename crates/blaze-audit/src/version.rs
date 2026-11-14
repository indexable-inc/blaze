//! Whether a locked version is affected by an advisory, and the version that fixes it.
//!
//! An advisory lists `patched` and `unaffected` ranges; a version is affected when it is in
//! none of them. A range is a semver requirement (`>= 1.2.3`, `^0.4.1`, `>= 1.0, < 1.5`), and
//! a version is in it when it satisfies every comparator **by order**: unlike cargo's rule
//! for choosing a version, a pre-release is not left out of a range that names no
//! pre-release, because `1.3.0-rc.1` after a `>= 1.2.3` fix has that fix. The comparators'
//! meaning is cargo's: `^` keeps the left-most non-zero part, `~` the minor (or the major when
//! only it is given), `=` and a wildcard every part given, a bare version is `^`.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use semver::{Comparator, Op, Prerelease, Version, VersionReq};

fn at(major: u64, minor: u64, patch: u64) -> Version {
    Version::new(major, minor, patch)
}

/// The least version: `major.minor.patch-0`, below every pre-release of it.
fn floor(major: u64, minor: u64, patch: u64) -> Version {
    Version { pre: Prerelease::new("0").expect("`0` is a pre-release"), ..at(major, minor, patch) }
}

/// One end of the versions a comparator admits: a version, and whether it is itself in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct End {
    pub at: Version,
    pub inclusive: bool,
}

fn inclusive(at: Version) -> Option<End> {
    Some(End { at, inclusive: true })
}

fn exclusive(at: Version) -> Option<End> {
    Some(End { at, inclusive: false })
}

/// The interval of versions `c` admits: its low and its high end, none where unbounded.
pub fn bounds(c: &Comparator) -> (Option<End>, Option<End>) {
    let (major, minor, patch) = (c.major, c.minor, c.patch);
    let given = Version { pre: c.pre.clone(), ..at(major, minor.unwrap_or(0), patch.unwrap_or(0)) };
    let whole = minor.is_some() && patch.is_some();
    // The least version past every version that agrees with `c` on the parts it gives.
    let past_given = || match (minor, patch) {
        (None, _) => floor(major + 1, 0, 0),
        (Some(minor), None) => floor(major, minor + 1, 0),
        (Some(minor), Some(patch)) => floor(major, minor, patch + 1),
    };
    let low_of_given = || if c.pre.is_empty() { floor(given.major, given.minor, given.patch) } else { given.clone() };
    match c.op {
        Op::Exact if whole => (inclusive(given.clone()), inclusive(given)),
        Op::Exact | Op::Wildcard => (inclusive(low_of_given()), exclusive(past_given())),
        Op::Greater if whole => (exclusive(given), None),
        Op::Greater => (inclusive(past_given()), None),
        Op::GreaterEq => (inclusive(given), None),
        Op::Less => (None, exclusive(given)),
        Op::LessEq if whole => (None, inclusive(given)),
        Op::LessEq => (None, exclusive(past_given())),
        Op::Tilde => {
            let high = match minor {
                None => floor(major + 1, 0, 0),
                Some(minor) => floor(major, minor + 1, 0),
            };
            (inclusive(given), exclusive(high))
        }
        Op::Caret => {
            let high = match (major, minor, patch) {
                (0, None, _) => floor(1, 0, 0),
                (0, Some(0), None) => floor(0, 1, 0),
                (0, Some(0), Some(patch)) => floor(0, 0, patch + 1),
                (0, Some(minor), _) => floor(0, minor + 1, 0),
                (major, _, _) => floor(major + 1, 0, 0),
            };
            (inclusive(given), exclusive(high))
        }
        // `Op` is non-exhaustive; a comparator of a kind this does not know admits nothing,
        // so a version is never called patched by a rule not read.
        _ => (exclusive(at(u64::MAX, u64::MAX, u64::MAX)), exclusive(floor(0, 0, 0))),
    }
}

/// Whether `version` satisfies `c`, by order.
pub fn admits(c: &Comparator, version: &Version) -> bool {
    let (low, high) = bounds(c);
    let above = low.is_none_or(|it| if it.inclusive { version >= &it.at } else { version > &it.at });
    let below = high.is_none_or(|it| if it.inclusive { version <= &it.at } else { version < &it.at });
    above && below
}

/// Whether `version` is in the range `req`: every comparator admits it.
pub fn within(req: &VersionReq, version: &Version) -> bool {
    req.comparators.iter().all(|c| admits(c, version))
}

/// Whether `version` is affected: in no `patched` and no `unaffected` range.
pub fn affected(patched: &[VersionReq], unaffected: &[VersionReq], version: &Version) -> bool {
    !patched.iter().chain(unaffected).any(|req| within(req, version))
}

/// Whether cargo counts `b` compatible with `a`: the same left-most non-zero part.
pub fn compatible(a: &Version, b: &Version) -> bool {
    match (a.major, a.minor) {
        (0, 0) => b.major == 0 && b.minor == 0 && b.patch == a.patch,
        (0, minor) => b.major == 0 && b.minor == minor,
        (major, _) => b.major == major,
    }
}

/// The fix for `version`: the least version above it that a `patched` range starts at, and
/// whether that is semver-compatible with `version` (so only the lock moves). The least is
/// the one to name: versions above `version` in its compatible line all come before every
/// version past that line, so a compatible fix, when there is one, is the least.
pub fn fix(patched: &[VersionReq], version: &Version) -> Option<(Version, bool)> {
    let mut least: Option<Version> = None;
    for req in patched {
        // Where the range starts: its highest low end, the pre-release floor `-0` dropped
        // (no one publishes `-0`; the release is what a person asks for).
        let Some(low) = req.comparators.iter().filter_map(|c| bounds(c).0.map(|it| it.at)).max() else {
            continue;
        };
        let low = if low.pre.as_str() == "0" { Version { pre: Prerelease::EMPTY, ..low } } else { low };
        if within(req, &low) && &low > version && least.as_ref().is_none_or(|it| &low < it) {
            least = Some(low);
        }
    }
    let first = least?;
    let same = compatible(version, &first);
    Some((first, same))
}
