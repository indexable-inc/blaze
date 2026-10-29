//! What the cargo drop-in asks of the engine (crates/blaze-engine), which depends on this crate:
//! the engine hands its functions over with [`set`] before it runs any command.

use std::{path::Path, sync::OnceLock};

pub struct Engine {
    /// `blaze doc`, which loads the workspace in its own process: its exit code.
    pub doc: fn(&[String]) -> anyhow::Result<i32>,
    /// What the stand-ins left in an executable say (`link::stand_ins` of the engine).
    pub stand_ins: fn(&Path) -> anyhow::Result<Vec<String>>,
    /// `blaze pin`, which writes this release into the workspace's manifest
    /// (`versions::pin` of the engine): its exit code.
    pub pin: fn(&[String]) -> anyhow::Result<i32>,
    /// Which blaze this is and why it is the one that runs, for `blaze --version`.
    pub why_this_one: fn() -> String,
    /// This blaze's version: a release's `0.YYYYMMDD.N`, or `0.0.0-dev` for a build of the
    /// tree. Said by the engine's build.rs alone (`BLAZE_RELEASE`), so that no release and no
    /// commit changes this crate.
    pub version: &'static str,
    /// What `--version` prints first: `blaze 0.20261028.9 (fb82b09 2026-10-28)`, or
    /// `blaze 0.0.0-dev (fb82b09)`.
    pub version_line: &'static str,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

pub fn set(engine: Engine) {
    let _ = ENGINE.set(engine);
}

pub fn get() -> &'static Engine {
    ENGINE.get().expect("blaze-engine's main hands over the engine before any command runs")
}

/// An engine for this crate's own tests, which have no blaze-engine to hand one over: what
/// needs the engine's code fails, and the version is a build of the tree's.
#[cfg(test)]
pub fn for_tests() {
    fn none(_: &[String]) -> anyhow::Result<i32> {
        anyhow::bail!("no engine in blaze-cargo's tests")
    }
    set(Engine {
        doc: none,
        stand_ins: |_| anyhow::bail!("no engine in blaze-cargo's tests"),
        pin: none,
        why_this_one: String::new,
        version: "0.0.0-dev",
        version_line: "blaze 0.0.0-dev",
    });
}
