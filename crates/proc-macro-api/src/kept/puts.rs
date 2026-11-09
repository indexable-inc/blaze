//! Expansions to keep, put by the thread that made them into a buffer of its own, never into
//! the store (docs/v2/plan/design/concurrency.md, "Writes"): an expansion runs inside a query
//! on a worker, many at once, and the store has one owner. The plane task that ran the query
//! takes its worker's puts as it ends ([`take_puts`]) and hands them to the store's owner with
//! its result, which keeps them ([`publish`]).
//!
//! A [`Keep::Durable`] put is the exception: it goes to its daemon's own queue
//! (`super::Keeping::put`), never to a thread's buffer nor to [`ORPHANS`], which any task's
//! take drains: a task of another job could take it and be descheduled while the job that made
//! it ends, so that job would be reported done before its record is durable. Only the store's
//! owner takes that queue (`super::Keeping::publish`), and it hands the ticket to the jobs not
//! ended as it does.
//!
//! A put is checked when it is kept, as it was when the expansion kept it itself: one whose
//! key the store has other bytes of is an error naming the expansion, and fails the build
//! ([`super::server_failed`]).

use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
};

use blaze_cas::{Key, Kind, Store, Ticket, Tickets, Unit};
use blaze_core::Traced;
use blaze_usercode::{
    RunKey,
    index::{self, KeepError},
    roots::verify,
};
use crossbeam_queue::SegQueue;

/// One value to keep: where it goes, and the value.
#[derive(Debug)]
pub struct Put {
    pub keep: Keep,
    pub value: Vec<u8>,
    /// Which value it is, as an error about it begins: "the expansion of `Serialize` of
    /// /lib.so".
    pub what: String,
}

impl Put {
    /// What [`blaze_usercode::roots::verify::keeps`] said to put, as a put of the expansion
    /// `what`: a record that must be durable ([`verify::Put::durable`], a retraction) is
    /// [`Keep::Durable`], so the job waits for it; every other is [`Keep::Run`].
    pub fn of(it: verify::Put, what: String) -> Put {
        let keep = match it.durable {
            true => Keep::Durable { run: it.run, traced: it.traced },
            false => Keep::Run { run: it.run, traced: it.traced },
        };
        Put { keep, value: it.value, what }
    }
}

/// Where a [`Put`] is kept.
#[derive(Debug)]
pub enum Keep {
    /// A run's result (an expansion, a verdict), found again by what it read
    /// ([`blaze_usercode::index`]).
    Run { run: RunKey, traced: Traced },
    /// [`Keep::Run`] of a record that must be durable before the job that put it is reported
    /// done: that an expansion verified across checkouts depends on where its roots are
    /// ([`blaze_usercode::roots::verify::retraction`]). A crash before it is durable would
    /// forget it, and the expansion would be given to every checkout again.
    Durable { run: RunKey, traced: Traced },
    /// Machine code the daemon compiled, under `key` as a [`Kind::Machine`]: the one kind a
    /// fetch from a remote never keeps, so a reader that finds one in the store's own
    /// directory knows this machine made it.
    Machine(Key),
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

/// The puts of threads that ended before they were taken, for the next [`take_puts`]: pushed
/// as a thread ends with puts left, and drained by a take that sees [`ORPHANED`]. One process
/// holds it, so with two daemons in it (the simulation) one may publish another's put into its
/// own store: a [`Keep::Run`] or [`Keep::Machine`] put is under the key of every input it was
/// made from (rule 21), so that is a value the other store lacks, never a wrong one. A
/// [`Keep::Durable`] put is never here (see the module's docs).
static ORPHANS: SegQueue<Put> = SegQueue::new();
static ORPHANED: AtomicBool = AtomicBool::new(false);

fn orphan(puts: impl IntoIterator<Item = Put>) {
    for put in puts {
        ORPHANS.push(put);
    }
    ORPHANED.store(true, Ordering::Release);
}

/// Leaves `put` for this thread's next [`take_puts`]. A [`Keep::Durable`] one is a bug of the
/// caller, a panic naming it (rule 20: it fails the one query): it goes through its daemon
/// (`super::Keeping::put`), as the module's docs say why.
pub fn put(put: Put) {
    if matches!(put.keep, Keep::Durable { .. }) {
        panic!(
            "{}: a durable put goes through its daemon's `Keeping::put`, not a thread's puts",
            put.what
        );
    }
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
    /// The tickets of its [`Keep::Durable`] puts, the last of each segment ([`Tickets`]): the
    /// job is not done until they are durable, and the store is flushed for them.
    pub durable: Tickets,
}

/// Keeps `puts` in `store`, in order. One the store can't keep is refused, naming it, and
/// told [`super::server_failed`]; the rest are kept still.
pub(crate) fn publish(store: &mut Store, puts: Vec<Put>) -> Published {
    let mut published = Published::default();
    for Put { keep, value, what } in puts {
        let durable = matches!(keep, Keep::Durable { .. });
        // Every put that landed, also of one that failed partway: its value is readable, so a
        // durable one is waited on all the same (a record visible to every checkout and lost
        // in a crash gives the value it retracts again).
        let mut tickets = Vec::with_capacity(2); // alloc: a put's two tickets, value and shape
        let kept = match keep {
            Keep::Run { run, traced } | Keep::Durable { run, traced } => {
                index::keep_into(store, Unit::SHARED, &run, &traced, &value, &mut tickets)
            }
            Keep::Machine(key) => store
                .put(key, Kind::Machine, &value)
                .map(|ticket| tickets.push(ticket))
                .map_err(KeepError::Mismatch),
        };
        if durable {
            for ticket in &tickets {
                published.durable.add(*ticket);
            }
        }
        published.tickets.extend(tickets);
        match kept {
            Ok(()) => {}
            Err(error) => {
                let said = refused(&what, &error);
                super::system(&said);
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
            "{what}: the store has another value under the same key of every input it read, \
             so the key misses an input or what made it is not deterministic: {mismatch:?}"
        ),
        // alloc: error path
        KeepError::NotACopy { key, why } => {
            format!("{what}: the store's {key:?}, another checkout's copy of it: {why}")
        }
        // alloc: error path
        KeepError::TooManyShapes { run } => {
            format!(
                "{what}: run {run:?} has more than {} shapes, so this one's is not kept",
                index::MOST_SHAPES
            )
        }
        // alloc: error path
        KeepError::Scan { run, why } => {
            format!("{what}: run {run:?}: scanning what it wrote for its roots: {why}")
        }
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
        let config =
            Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        }).expect("the effects end");
        assert!(store.ready());
        store
    }

    fn run() -> RunKey {
        RunKey {
            code: ContentId::of(super::super::LIBRARY, b"lib"),
            given: ContentId::of("test", b"call"),
        }
    }

    fn one(value: &[u8], what: &str) -> Put {
        Put {
            keep: Keep::Run {
                run: run(),
                traced: Traced {
                    files: vec![(PathBuf::from("/a"), Some([1; 32]))],
                    ..Traced::default()
                },
            },
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
        struct Same;
        impl blaze_usercode::trace::Inputs for Same {
            type Error = String;
            fn entry(
                &mut self,
                _: &std::path::Path,
            ) -> Result<blaze_usercode::trace::Entry, String> {
                Ok(blaze_usercode::trace::Entry::Missing)
            }
            fn file(&mut self, _: &std::path::Path) -> Result<Option<[u8; 32]>, String> {
                Ok(Some([1; 32]))
            }
            fn dir(&mut self, _: &std::path::Path) -> Result<Option<[u8; 32]>, String> {
                Ok(None)
            }
            fn names(
                &mut self,
                _: &std::path::Path,
                _: &mut blaze_usercode::trace::Names<'_>,
            ) -> Result<bool, String> {
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
        match index::find(&reader, &guard, &run(), &mut Same) {
            Ok(Found::Hit { value, .. }) => assert_eq!(value, b"out"),
            other => panic!("{other:?}"),
        }
        drop(guard);
        let published = publish(&mut store, vec![one(b"other", "the expansion of `M` of /lib.so")]);
        assert_eq!(published.refused.len(), 1);
        assert!(
            published.refused[0]
                .starts_with("the expansion of `M` of /lib.so: the store has another")
        );
        assert!(super::super::server_failed().is_some());
    }

    /// D1 (round 5): a record that a value verified across checkouts depends on where its
    /// roots are, as `verify::keeps` says to put it (another checkout's copy differed), is
    /// published durable: the job waits for it, so a crash can't forget it and give the value
    /// to every checkout again.
    #[test]
    fn a_record_verify_says_to_put_is_published_durable() {
        let mut sim = Sim::new(7, Faults::default());
        let mut store = opened(&mut sim);
        let unmarked = blaze_cas::Key(ContentId::of("test", b"unmarked"));
        let record = verify::retraction(&run(), &unmarked);
        assert!(record.durable, "a retraction is a record that must be durable");
        let here = verify::Put {
            run: run(),
            traced: Traced {
                files: vec![(PathBuf::from("/a"), Some([1; 32]))],
                ..Traced::default()
            },
            value: b"out".to_vec(),
            durable: false,
        };
        let puts = vec![
            Put::of(record, "the record of `M`".to_owned()),
            Put::of(here, "the expansion of `M`".to_owned()),
        ];
        assert!(
            matches!(puts[0].keep, Keep::Durable { .. })
                && matches!(puts[1].keep, Keep::Run { .. })
        );
        let published = publish(&mut store, puts);
        assert!(published.refused.is_empty(), "{:?}", published.refused);
        let ticket =
            published.durable.iter().next().expect("the record's ticket is what the job waits for");
        assert_eq!(
            published.tickets.first(),
            Some(&ticket),
            "the record's own ticket, not the expansion's"
        );
    }

    /// C3 (round 4): a retraction's ticket is handed back for the job to wait on, and once the
    /// store says it is durable it survives a restart.
    #[test]
    fn a_retraction_is_durable_before_the_run_is_reported() {
        let mut sim = Sim::new(6, Faults::default());
        let mut store = opened(&mut sim);
        let unmarked = blaze_cas::Key(ContentId::of("test", b"unmarked"));
        let record = blaze_usercode::roots::verify::retraction(&run(), &unmarked);
        let put = Put {
            keep: Keep::Durable { run: record.run, traced: record.traced },
            value: record.value,
            what: "the retraction of `M` of /lib.so".to_owned(),
        };
        let published = publish(&mut store, vec![one(b"out", "kept"), put]);
        assert!(published.refused.is_empty(), "{:?}", published.refused);
        let ticket = published.durable.iter().next().expect("a retraction's ticket is handed back");
        assert!(published.tickets.contains(&ticket));
        // Not reported done yet: nothing was flushed.
        assert!(!store.durable(ticket));
        let mut effects = Vec::new();
        store.flush(&mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            _ = store.complete(completion, effects);
        }).expect("the effects end");
        assert!(store.durable(ticket), "a flushed retraction is durable");
        drop(store);
        // Restored, the record is there: no checkout is given the verified value again.
        let store = opened(&mut sim);
        let depends = index::depends_key(&run(), &unmarked);
        assert!(store.get(&depends).unwrap().is_some(), "a durable retraction survives a restart");
    }
}
