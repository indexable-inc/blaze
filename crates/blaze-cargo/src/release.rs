//! Which blaze this is: said by the program's build script alone (`BLAZE_RELEASE`,
//! crates/blaze-engine/build.rs) and handed over by its `main` with [`set`] before any command
//! runs, so that no release and no commit changes this crate.

use std::sync::OnceLock;

pub struct Release {
    /// A release's `0.YYYYMMDD.N`, or `0.0.0-dev` for a build of the tree (`BLAZE_VERSION`).
    pub version: &'static str,
    /// What `--version` prints: `blaze 0.20261028.9 (fb82b09 2026-10-28)`, or
    /// `blaze 0.0.0-dev (fb82b09)` (`BLAZE_VERSION_LINE`).
    pub line: &'static str,
}

static RELEASE: OnceLock<Release> = OnceLock::new();

pub fn set(release: Release) {
    let _ = RELEASE.set(release);
}

pub fn get() -> &'static Release {
    RELEASE.get().expect("the program's main hands over its release before any command runs")
}

/// The release of this crate's own tests, which have no program to hand one over: a build of
/// the tree's.
#[cfg(test)]
pub fn for_tests() {
    set(Release { version: "0.0.0-dev", line: "blaze 0.0.0-dev" });
}
