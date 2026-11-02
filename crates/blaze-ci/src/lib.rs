//! The VM as CI (docs/v2/plan/lanes/ci.md section 10).
//!
//! A GitHub webhook ([`Event`]) opens a view of the pushed commit on the running daemon and
//! sends it agent-API requests ([`blaze_api::Request`]): no runner, no cache transfer, no
//! workspace load. Results go back as check runs ([`CheckRun`]), so required checks and
//! `land.py` keep working. The release train runs one at a time; a newer tag replaces the
//! waiting one ([`Train`]).
//!
//! A webhook is trusted only after its signature checks against the App's secret; a fork's
//! PR runs with every build script, proc macro and test sandboxed and no secret readable.
//!
//! - [`webhook`]: a delivery's signature checked, its payload read into an [`Event`].
//! - [`github`]: the App's token and the check runs, over TLS.
//! - [`checkout`]: a commit's tree, from the VM's clone, as ops on the daemon's VFS.
//! - [`service`]: what each event makes happen, as a state machine; [`runtime`] runs it.
//! - [`oracle`]: which views the production oracle rebuilds from nothing.
//! - [`snapshot`] and [`standby`]: the VM snapshots itself; a standby resumes from the newest.
//! - [`config`]: the service's one JSON input.

#![forbid(unsafe_code)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub mod checkout;
pub mod config;
pub mod github;
pub mod oracle;
pub mod runtime;
pub mod service;
pub mod snapshot;
pub mod standby;
pub mod webhook;

use blaze_ids::ContentId;
use serde::{Deserialize, Serialize};

/// Holds this thread for `wait`: the one timer of blaze-ci's drivers, for the waits whose end
/// no event says (a post's backoff, the standby's next poll, an accept that failed). Each
/// caller's schedule decides the wait.
#[allow(clippy::disallowed_methods, reason = "the drivers' timer: no event ends these waits, and the callers' schedules decide them")]
pub(crate) fn hold(wait: std::time::Duration) {
    std::thread::sleep(wait);
}

/// What GitHub said happened, after its signature was checked.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub enum Event {
    Push { branch: String, commit: String },
    PullRequest { number: u64, head: String, base: String, from_fork: bool },
    Tag { name: String, commit: String },
}

/// A check run as GitHub's API takes it.
// clone: once per check-run post, the copy handed to the GitHub lane
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct CheckRun {
    pub name: String,
    pub commit: String,
    pub status: CheckStatus,
    pub summary: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckStatus {
    Queued,
    InProgress,
    Success,
    Failure,
    Cancelled,
}

/// The release train: at most one running and one waiting; a newer tag replaces the waiting.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct Train {
    pub running: Option<String>,
    pub waiting: Option<String>,
}

impl Train {
    /// A new tag arrived; returns the tag it replaced in waiting, if any.
    pub fn arrive(&mut self, tag: String) -> Option<String> {
        if self.running.is_none() {
            self.running = Some(tag);
            return None;
        }
        self.waiting.replace(tag)
    }

    /// The running tag ended; the waiting one, if any, starts and is returned.
    pub fn finish(&mut self) -> Option<&str> {
        self.running = self.waiting.take();
        self.running.as_deref()
    }
}

/// A commit's tree as a root of the daemon's VFS.
// clone: once per checkout, the tree a job starts from
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Checkout {
    pub commit: String,
    pub root: ContentId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_tag_runs_at_once() {
        let mut train = Train::default();
        assert_eq!(train.arrive("v1".to_owned()), None);
        assert_eq!(train.running.as_deref(), Some("v1"));
        assert_eq!(train.waiting, None);
    }

    #[test]
    fn a_newer_tag_replaces_the_waiting_one() {
        let mut train = Train::default();
        train.arrive("v1".to_owned());
        assert_eq!(train.arrive("v2".to_owned()), None);
        assert_eq!(train.arrive("v3".to_owned()), Some("v2".to_owned()));
        assert_eq!(train.running.as_deref(), Some("v1"));
        assert_eq!(train.waiting.as_deref(), Some("v3"));
    }

    #[test]
    fn finishing_starts_the_waiting_tag() {
        let mut train = Train::default();
        train.arrive("v1".to_owned());
        train.arrive("v2".to_owned());
        train.arrive("v3".to_owned());
        assert_eq!(train.finish(), Some("v3"));
        assert_eq!(train.waiting, None);
        assert_eq!(train.finish(), None);
        assert_eq!(train, Train::default());
    }
}
