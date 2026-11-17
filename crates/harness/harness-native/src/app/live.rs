//! Edits to the app's own code while it runs (crates/blaze-live): started with `blaze run`, the app takes each function
//! the daemon recompiles after a `blaze build`, between two frames, and keeps its world, windows and sessions. One static
//! image, every module linked in: blaze patches its jump table, so no module boundary, dylib or loader is needed.
//!
//! A frame's [`Live::poll`] applies the patches (the frame's start is the main thread's quiescent point: nothing of the
//! world's is half done). The window sleeps while idle, so a thread waits on the daemon's announcements and wakes it.
//! An edit no patch can take (a type the world holds values of changed) asks for a restart, which is logged.
use super::*;
use blaze_live::{Live, Outcome};

/// How long the waking thread sleeps between looks when no announcement wakes it (on macOS the daemon's announce does).
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

pub(super) struct Patches {
    live: Live,
}

impl Patches {
    /// Ready to take patches when the daemon started the app on its code memory (`blaze run`); `None`, said once, in any
    /// other start (a bundle, cargo, a shot), whose code is the file's.
    pub(super) fn start(wake: Wake) -> Option<Self> {
        let live = match Live::start() {
            Ok(live) => live,
            Err(why) => {
                eprintln!("harness-native: no live edits: {why}");
                return None;
            }
        };
        let announcements = live.announcements();
        let waking = move || {
            let mut seen = announcements.announced();
            loop {
                let now = announcements.wait(seen, LOOK_EVERY);
                if now != seen {
                    seen = now;
                    wake();
                }
            }
        };
        if let Err(e) = std::thread::Builder::new().name("live-edits-wake".to_owned()).spawn(waking) {
            // The frames still poll: a patch waits for the next frame an event or the daemon draws.
            eprintln!("harness-native: live edits: the thread that wakes the window for a patch: {e}");
        }
        eprintln!("harness-native: live edits on, at generation {}", live.generation());
        Some(Self { live })
    }

    /// Takes what came since the last frame; whether the app changed (the frame then draws on).
    pub(super) fn take(&mut self) -> bool {
        let Some(outcome) = self.live.poll() else { return false };
        eprintln!("harness-native: live edits: {outcome}");
        matches!(outcome, Outcome::Applied { .. } | Outcome::RolledBack { .. })
    }
}

impl App {
    /// The frame's first step: patches the daemon has for the app, applied before any of the frame's code runs.
    pub(super) fn take_patches(&mut self) {
        if self.patches.as_mut().is_some_and(Patches::take) {
            self.poke_why("live");
        }
    }
}
