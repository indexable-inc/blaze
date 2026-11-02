//! The code generator's memo across jobs ([`Memos`]): every build of a job reads and puts in
//! the one memo the job took at its start ([`Memos::for_job`]), and a job's start drops what
//! no kept front end's last build used ([`Held`]). A dropped entry costs a recompile or a
//! store read, never a wrong template: the store keeps every template by its stored key.

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_codegen::memo::Memo;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use parking_lot::{Mutex, MutexGuard};

/// How many entries the memo may hold past what the kept front ends used before a job's start
/// collects it: a collection is a pass over every entry, so it runs only once there is this
/// much to drop.
pub(crate) const SLACK: usize = 1 << 16;

/// The daemon's one memo, and what each kept front end's last build used of it.
pub(crate) struct Memos {
    memo: Mutex<Arc<Memo>>,
    kept: Mutex<KeptSets>,
}

#[derive(Default)]
struct KeptSets {
    next: u64,
    sets: HashMap<u64, Box<[ContentId]>>,
    /// The sum of the sets' lengths, at least their union's.
    total: usize,
}

/// A kept front end's hold on what its last build used of the memo: every collection keeps it
/// while the hold lives.
pub(crate) struct Held {
    memos: Arc<Memos>,
    id: u64,
}

impl Memos {
    pub(crate) fn new() -> Arc<Memos> {
        // alloc: once a daemon
        Arc::new(Memos { memo: Mutex::new(blaze_codegen::driver::shared_memo()), kept: Mutex::default() })
    }

    /// The memo for a job about to start, collected first when it holds [`SLACK`] entries more
    /// than the kept front ends used and nothing else holds it (no build or job in flight,
    /// which `retain` taking it alone rules out).
    pub(crate) fn for_job(&self) -> Arc<Memo> {
        let mut memo = lock(&self.memo);
        let kept = lock(&self.kept);
        if memo.len() > kept.total.saturating_add(SLACK)
            && let Some(only) = Arc::get_mut(&mut memo)
        {
            let union = kept.union();
            drop(kept);
            only.retain(|it| union.binary_search(it).is_ok());
        }
        Arc::clone(&memo)
    }

    /// Keeps `used`, every memo entry a kept front end's last build took or put, until the
    /// hold is dropped.
    pub(crate) fn hold(self: &Arc<Memos>, mut used: Vec<ContentId>) -> Held {
        used.sort_unstable();
        used.dedup();
        let mut kept = lock(&self.kept);
        let id = kept.next;
        kept.next += 1;
        kept.total += used.len();
        kept.sets.insert(id, used.into_boxed_slice());
        Held { memos: Arc::clone(self), id }
    }
}

impl KeptSets {
    /// Every kept set's entries, sorted, each once.
    fn union(&self) -> Vec<ContentId> {
        // alloc: once a collection
        let mut all = Vec::with_capacity(self.total);
        for set in self.sets.values() {
            all.extend_from_slice(set);
        }
        all.sort_unstable();
        all.dedup();
        all
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut kept = lock(&self.memos.kept);
        if let Some(set) = kept.sets.remove(&self.id) {
            kept.total -= set.len();
        }
    }
}

fn lock<T>(it: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing is left half-changed under either lock by a panic: each change is one step.
    it.lock()
}

#[cfg(test)]
mod tests {
    use blaze_codegen::{Arch, Relocs, template::Template};

    use super::*;

    fn shape(n: u32) -> ContentId {
        ContentId::of("blaze-cargo memo test", &n.to_le_bytes())
    }

    fn template(n: u32) -> Template {
        Template {
            arch: Arch::X86_64,
            align: 1,
            code: n.to_le_bytes().to_vec(),
            relocs: Relocs::default(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            refs: 0,
            externs: Vec::new(),
            coverage: None,
            profile: None,
        }
    }

    fn fill(memo: &Memo, from: u32, count: u32) {
        for n in from..from + count {
            memo.put(shape(n), template(n));
        }
    }

    /// Past the slack, a job's start drops what no kept front end used and keeps what one
    /// does; a memo another job still holds, or one within the slack, is left as it is.
    #[test]
    fn a_jobs_start_drops_what_no_kept_front_end_used() {
        let memos = Memos::new();
        let first = memos.for_job();
        let count = SLACK as u32 + 8;
        fill(&first, 0, count);
        let kept = memos.hold((0..4).map(shape).collect());
        let other = memos.hold((4..6).map(shape).collect());

        let held = memos.for_job();
        assert_eq!(held.len(), count as usize, "the first job still holds the memo: nothing is collected");
        drop((first, held));

        drop(other);
        let next = memos.for_job();
        assert_eq!(next.len(), 4, "only what the kept front end used is left");
        for n in 0..4 {
            assert_eq!(next.get(&shape(n)).map(|it| it.code.as_slice()), Some(&n.to_le_bytes()[..]));
        }
        assert!(next.get(&shape(4)).is_none(), "a dropped hold keeps nothing");
        drop(next);

        let again = memos.for_job();
        fill(&again, 100, 8);
        drop(again);
        assert_eq!(memos.for_job().len(), 12, "within the slack, nothing is collected");
        drop(kept);
        assert_eq!(lock(&memos.kept).total, 0);
    }
}
