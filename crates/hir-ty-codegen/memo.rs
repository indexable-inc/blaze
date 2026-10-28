//! What Cranelift made in this process, by the key of the IR it made it from.
//!
//! Instances of one generic function mostly differ only in the functions they call
//! (`Vec<A>::reserve` calls `Vec<A>::grow`, `Vec<B>::reserve` calls `Vec<B>::grow`), and the IR
//! names those by their place among the function's names, not by what they are. Cranelift's key
//! of a function is all of its IR but what those names stand for, with the target's flags: two
//! functions with one key compile to the same code, with each name put back for its own. The
//! store finds only what is in its sealed packs, which are sealed between builds, so without
//! this a cold build compiles each sibling again. (Rik Arends' Relax does the same for its own
//! back end: `libs/relax/compiler/src/bmemo.rs`.)
//!
//! Kept in two generations of [`GENERATION`] bytes: when the newer fills, the older goes.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rustc_hash::FxHashMap;

use super::store::KEY;

const GENERATION: usize = 64 << 20;

#[derive(Default)]
struct Memo {
    now: FxHashMap<[u8; KEY], Entry>,
    old: FxHashMap<[u8; KEY], Entry>,
    bytes: usize,
}

#[derive(Clone)]
struct Entry {
    value: Arc<[u8]>,
    /// What compiling it took, which each time it is found saves.
    took: Duration,
}

static MEMO: Mutex<Memo> = Mutex::new(Memo {
    now: FxHashMap::with_hasher(rustc_hash::FxBuildHasher),
    old: FxHashMap::with_hasher(rustc_hash::FxBuildHasher),
    bytes: 0,
});

/// Found, kept, nanoseconds of compiling the found saved, generations dropped.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn memo() -> std::sync::MutexGuard<'static, Memo> {
    MEMO.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn get(key: &[u8; KEY]) -> Option<Arc<[u8]>> {
    let mut memo = memo();
    let entry = match memo.now.get(key) {
        Some(entry) => entry.clone(),
        None => {
            let entry = memo.old.remove(key)?;
            memo.keep(*key, entry.clone());
            entry
        }
    };
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    COUNTS[2].fetch_add(entry.took.as_nanos() as u64, Ordering::Relaxed);
    Some(entry.value)
}

pub(super) fn insert(key: &[u8; KEY], value: Vec<u8>, took: Duration) {
    COUNTS[1].fetch_add(1, Ordering::Relaxed);
    memo().keep(*key, Entry { value: value.into(), took });
}

impl Memo {
    fn keep(&mut self, key: [u8; KEY], entry: Entry) {
        let bytes = entry.value.len() + KEY;
        if self.bytes + bytes > GENERATION {
            COUNTS[3].fetch_add(1, Ordering::Relaxed);
            self.old = std::mem::take(&mut self.now);
            self.bytes = 0;
        }
        self.bytes += bytes;
        if let Some(was) = self.now.insert(key, entry) {
            self.bytes -= was.value.len() + KEY;
        }
    }
}

/// Says what was found and kept since the last call, on stderr.
pub(super) fn report() {
    let [found, kept, saved, dropped] = COUNTS.each_ref().map(|it| it.swap(0, Ordering::Relaxed));
    if found + kept == 0 {
        return;
    }
    let held = memo().bytes as f64 / 1e6;
    let saved = Duration::from_nanos(saved);
    eprintln!(
        "memo: {found} functions found in this process, {kept} compiled and kept, \
         {saved:.1?} of Cranelift saved, {dropped} generations dropped, {held:.1} MB held"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_generation_becomes_the_old_one_and_what_is_found_there_is_kept_again() {
        let mut memo = Memo::default();
        let entry = |len| Entry { value: vec![0; len].into(), took: Duration::ZERO };
        let half = GENERATION / 2;
        memo.keep([1; KEY], entry(half));
        memo.keep([2; KEY], entry(half));
        assert!(memo.old.contains_key(&[1; KEY]) && memo.now.contains_key(&[2; KEY]));
        memo.keep([3; KEY], entry(half));
        assert!(memo.old.contains_key(&[2; KEY]) && !memo.old.contains_key(&[1; KEY]));
        assert_eq!(memo.bytes, half + KEY);
    }
}
