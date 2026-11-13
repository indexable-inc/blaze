//! The one way the jobs ask the driver to write a file ([`ViewWrite`]).
//!
//! The class it closes (rule guard-the-cause): a file the daemon writes into a checkout it
//! watches comes back from the watcher as a change someone else made, and the next build is
//! of a view no build has seen, unless the build that wrote the file holds it in its own view
//! first. Until 2026-11-12 the lock files a plan writes were written to the disk alone.
//!
//! So a write is a value only [`ViewWrite::new`] makes, and it makes one only for a path
//! that is in no watched checkout's view, or that the build's own view already holds with the
//! bytes to be written. [`ViewWrite::effect`] is the one `Effect::Write` of the jobs (the
//! no-op lane's test `the_daemon_writes_into_a_checkout_only_through_the_builds_view` holds
//! the source to that), so a write into a watched root that skipped the view cannot be asked:
//! it is refused where it is made, with an error naming the file and the checkout.

use blaze_core::{Effect, FileName, Tag};
use blaze_ids::ContentId;
use blaze_vfs::{Vfs, View};

/// A file's write the driver may be asked for: its fields are private, so only
/// [`ViewWrite::new`] makes one.
pub(super) struct ViewWrite {
    file: FileName,
    bytes: Vec<u8>,
}

impl ViewWrite {
    /// The write of `bytes` at `path` (absolute; `file` as the driver names it), by a build
    /// whose view is the root `view` of the checkout at `view_dir`, while `checkouts` are
    /// the checkouts watched now, each by its absolute path.
    ///
    /// Made when `path` is in no watched checkout's view (outside all of them, or under what
    /// a view leaves out: `blaze_vfs::watch::fixed_out`), or when it is a file of the build's own
    /// checkout and `view` holds it with exactly `bytes`: then the watcher's report of the
    /// write is of a file the build's view has. Else the reason: the write would come back
    /// as a change to a view that does not hold it.
    pub(super) fn new<'c>(
        vfs: &mut Vfs,
        mut checkouts: impl Iterator<Item = &'c str>,
        view_dir: &str,
        view: ContentId,
        path: &str,
        file: FileName,
        bytes: Vec<u8>,
    ) -> Result<ViewWrite, String> {
        let rel_under = |dir: &str| {
            let rest = path.strip_prefix(dir.trim_end_matches('/'))?.strip_prefix('/')?;
            (!blaze_vfs::watch::fixed_out(rest)).then_some(rest)
        };
        let Some(watched) = checkouts.find(|dir| rel_under(dir).is_some()) else {
            return Ok(ViewWrite { file, bytes });
        };
        let own = watched.trim_end_matches('/') == view_dir.trim_end_matches('/');
        let held = own
            && rel_under(view_dir).is_some_and(|rel| match vfs.open(View { root: view }) {
                Ok(opened) => {
                    let same = matches!(
                        vfs.read(opened, rel),
                        Ok(Some(had)) if had.bytes == bytes.as_slice()
                    );
                    vfs.close(opened);
                    same
                }
                Err(_) => false,
            });
        if held {
            return Ok(ViewWrite { file, bytes });
        }
        // alloc: an error, once
        Err(format!(
            "{path} is a file of the watched checkout {watched}, and the view of the build that writes it \
             ({view_dir}, root {view:x}) does not hold it as it would be written: the watcher would bring the \
             write back as a change (a file blaze writes into a checkout goes into the build's view first)"
        ))
    }

    /// The write, as the driver is asked for it.
    pub(super) fn effect(self, tag: Tag) -> Effect {
        // durable: the user's lock file: their own source, written whole or not at all
        Effect::Write { file: self.file, bytes: self.bytes, tag }
    }
}
