//! Which blaze this is: said by the program's build script alone (`BLAZE_RELEASE`,
//! crates/blaze-engine/build.rs) and handed over by its `main` with [`set`] before any command
//! runs, so that no release and no commit changes this crate.

/// Where blaze's releases are: `<this>/v<version>/<asset>` for each asset of a release (its
/// archives, `SHA256SUMS`, `B3SUMS`, the library's source), `<this>/channels/<channel>` for the
/// channels' pointers and `<this>/latest.json`. The R2 bucket `blaze-releases` on its custom
/// domain (docs/release.md, "Where the bytes are"), which release.yaml fills before it makes the
/// GitHub release: the one place blaze fetches a release from, GitHub's copy is no fallback.
/// `BLAZE_RELEASES` names another place laid out the same (blaze-engine's `versions.rs`).
pub const RELEASES: &str = "https://blaze-releases.ix.dev";

pub struct Release {
    /// A release's `0.YYYYMMDD.N`, or `0.0.0-dev` for a build of the tree (`BLAZE_VERSION`).
    pub version: &'static str,
    /// What `--version` prints: `blaze 0.20261028.9 (fb82b09 2026-10-28)`, or
    /// `blaze 0.0.0-dev (fb82b09)` (`BLAZE_VERSION_LINE`).
    pub line: &'static str,
}

#[expect(
    clippy::disallowed_types,
    reason = "which blaze the process is, handed over once by the program's main so that no release changes this crate"
)]
static RELEASE: std::sync::OnceLock<Release> = std::sync::OnceLock::new();

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

/// What the daemon runs off its loop to keep blaze's update state fresh, handed over by the
/// program's main (blaze-engine's `update.rs`, which owns the network, the files and the
/// decisions): it checks `latest.json` when due and says how long until it is due again.
#[expect(
    clippy::disallowed_types,
    reason = "a function handed over once by the program's main, so that this crate has no update code"
)]
static UPDATES: std::sync::OnceLock<fn() -> std::time::Duration> = std::sync::OnceLock::new();

pub fn set_updates(check: fn() -> std::time::Duration) {
    let _ = UPDATES.set(check);
}

/// How long the update thread waits after a check that panicked, which failed that check alone.
const AFTER_PANIC: std::time::Duration = std::time::Duration::from_secs(300);

/// The daemon's update thread, which ends when this is dropped (the daemon ends).
pub struct Updates {
    _stop: crossbeam_channel::Sender<()>,
}

/// Starts the daemon's update thread when the program handed over a check ([`set_updates`]):
/// it runs the check, then waits as long as the check said, or until the returned [`Updates`]
/// is dropped. Each check and each wait is a span naming it. A check that panics fails only
/// itself; the thread goes on after [`AFTER_PANIC`].
pub fn watch_updates() -> std::io::Result<Option<Updates>> {
    let Some(&check) = UPDATES.get() else { return Ok(None) };
    let (stop, stopped) = crossbeam_channel::bounded::<()>(0);
    std::thread::Builder::new()
        .name("blaze-update-check".to_owned()) // alloc: once a daemon
        .spawn(move || {
            loop {
                let wait = {
                    let _span = tracing::info_span!("update check: reading latest.json").entered();
                    std::panic::catch_unwind(check).unwrap_or(AFTER_PANIC)
                };
                let _span = tracing::info_span!(
                    "update check: waiting for the next",
                    secs = wait.as_secs()
                )
                .entered();
                match stopped.recv_timeout(wait) {
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    // The daemon ended: its `Updates` was dropped.
                    Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;
    Ok(Some(Updates { _stop: stop }))
}
