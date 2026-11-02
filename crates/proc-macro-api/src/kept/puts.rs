//! Expansions to keep, put by the thread that made them into a buffer of its own, never into
//! the store (docs/v2/plan/design/concurrency.md, "Writes"): an expansion runs inside a query
//! on a worker, many at once, and the store has one owner. The plane task that ran the query
//! takes its worker's puts as it ends ([`take_puts`]) and hands them to the store's owner with
//! its result, which keeps them ([`publish`]).
//!
//! A put is checked when it is kept, as it was when the expansion kept it itself: one whose
//! key the store has other bytes of is an error naming the expansion, and fails the build
//! ([`super::server_failed`]).

use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
};

use blaze_cas::{Store, Ticket};
use blaze_core::Traced;
use blaze_usercode::{
    RunKey,
    index::{self, KeepError},
};
use crossbeam_queue::SegQueue;

/// One expansion to keep: its run, what it read, and the value kept of it.
#[derive(Debug)]
pub struct Put {
    pub run: RunKey,
    pub traced: Traced,
    pub value: Vec<u8>,
    /// Which expansion it is, as an error about it begins: "the expansion of `Serialize` of
    /// /lib.so".
    pub what: String,
}

/// A thread's puts not taken yet.
struct Buffer(RefCell<Vec<Put>>);

impl Drop for Buffer {
    fn drop(&mut self) {
        let puts = std::mem::take(self.0.get_mut());
        if !puts.is_empty() {
            orphan(puts);
        }
    }
}

thread_local! {
    static PUTS: Buffer = const { Buffer(RefCell::new(Vec::new())) }; // alloc: none, an empty buffer
}

/// The puts of threads that ended before they were taken, for the next [`take_puts`]. Pushed
/// only as a thread ends with puts left, and drained by a take that sees [`ORPHANED`].
static ORPHANS: SegQueue<Put> = SegQueue::new();
static ORPHANED: AtomicBool = AtomicBool::new(false);

fn orphan(puts: impl IntoIterator<Item = Put>) {
    for put in puts {
        ORPHANS.push(put);
    }
    ORPHANED.store(true, Ordering::Release);
}

/// Leaves `put` for this thread's next [`take_puts`].
pub fn put(put: Put) {
    let mut put = Some(put);
    _ = PUTS.try_with(|it| it.0.borrow_mut().extend(put.take()));
    // This thread is ending: its buffer is gone.
    if let Some(put) = put {
        orphan([put]);
    }
}

/// This thread's puts since its last take, and those of threads that ended with puts left, in
/// the order each thread made them.
pub fn take_puts() -> Vec<Put> {
    let mut puts = PUTS.try_with(|it| std::mem::take(&mut *it.0.borrow_mut())).unwrap_or_default();
    if ORPHANED.load(Ordering::Acquire) && ORPHANED.swap(false, Ordering::AcqRel) {
        puts.extend(std::iter::from_fn(|| ORPHANS.pop()));
    }
    puts
}

/// What [`publish`] did.
#[derive(Debug, Default)]
pub struct Published {
    /// The tickets of what it put, durable once the store says so.
    pub tickets: Vec<Ticket>,
    /// What it could not keep, each naming its expansion; the first fails the build.
    pub refused: Vec<String>,
}

/// Keeps `puts` in `store`, in order. One the store can't keep is refused, naming it, and
/// told [`super::server_failed`]; the rest are kept still.
pub fn publish(store: &mut Store, puts: Vec<Put>) -> Published {
    let mut published = Published::default();
    for Put { run, traced, value, what } in puts {
        match index::keep(store, &run, &traced, value) {
            Ok(tickets) => published.tickets.extend(tickets),
            Err(error) => {
                let said = refused(&what, &error);
                super::system(said.clone()); // clone: error path, a refused put, said and kept
                published.refused.push(said);
            }
        }
    }
    published
}

fn refused(what: &str, error: &KeepError) -> String {
    match error {
        KeepError::Corrupt(corrupt) => format!("{what}: {corrupt}"), // alloc: error path
        // alloc: error path
        KeepError::Mismatch(mismatch) => format!(
            "{what}: the store has another expansion under the same key of every input it read, \
             so the key misses an input or the macro is not deterministic: {mismatch:?}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use blaze_cas::Config;
    use blaze_core::{
        FileName, run_until_idle,
        sim::{Faults, Sim},
    };
    use blaze_ids::ContentId;
    use blaze_usercode::index::Found;

    use super::*;

    fn opened(sim: &mut Sim) -> Store {
        let mut effects = Vec::new();
        let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        });
        assert!(store.ready());
        store
    }

    fn one(value: &[u8], what: &str) -> Put {
        Put {
            run: RunKey { code: ContentId::of(super::super::LIBRARY, b"lib"), given: ContentId::of("test", b"call") },
            traced: Traced { files: vec![(PathBuf::from("/a"), Some([1; 32]))], ..Traced::default() },
            value: value.to_vec(),
            what: what.to_owned(),
        }
    }

    #[test]
    fn a_put_is_this_threads_until_taken_and_one_left_by_an_ended_thread_is_taken_too() {
        _ = take_puts();
        put(one(b"mine", "mine"));
        std::thread::spawn(|| put(one(b"left", "left"))).join().unwrap();
        let mut taken: Vec<String> = take_puts().into_iter().map(|it| it.what).collect();
        taken.sort();
        assert_eq!(taken, ["left", "mine"]);
        assert!(take_puts().is_empty());
    }

    #[test]
    fn a_published_put_is_found_and_another_value_of_its_key_is_refused_by_name() {
        let mut sim = Sim::new(5, Faults::default());
        let mut store = opened(&mut sim);
        let published = publish(&mut store, vec![one(b"out", "first"), one(b"out", "again")]);
        assert!(published.refused.is_empty(), "{:?}", published.refused);
        let put = one(b"out", "x");
        struct Same;
        impl blaze_usercode::trace::Inputs for Same {
            type Error = String;
            fn entry(&mut self, _: &std::path::Path) -> Result<blaze_usercode::trace::Entry, String> {
                Ok(blaze_usercode::trace::Entry::Missing)
            }
            fn file(&mut self, _: &std::path::Path) -> Result<Option<[u8; 32]>, String> {
                Ok(Some([1; 32]))
            }
            fn dir(&mut self, _: &std::path::Path) -> Result<Option<[u8; 32]>, String> {
                Ok(None)
            }
            fn names(&mut self, _: &std::path::Path, _: &mut blaze_usercode::trace::Names<'_>) -> Result<bool, String> {
                Ok(false)
            }
            fn env(&mut self, _: &std::ffi::OsStr) -> Option<&std::ffi::OsStr> {
                None
            }
            fn env_all(&mut self) -> &[(std::ffi::OsString, std::ffi::OsString)] {
                &[]
            }
        }
        let (reader, guard) = (store.reader(), blaze_cas::enter());
        match index::find(&reader, &guard, &put.run, &mut Same) {
            Ok(Found::Hit { value, .. }) => assert_eq!(value, b"out"),
            other => panic!("{other:?}"),
        }
        drop(guard);
        let published = publish(&mut store, vec![one(b"other", "the expansion of `M` of /lib.so")]);
        assert_eq!(published.refused.len(), 1);
        assert!(published.refused[0].starts_with("the expansion of `M` of /lib.so: the store has another"));
        assert!(super::super::server_failed().is_some());
    }
}
