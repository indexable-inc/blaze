//! The lazy run's compiler over a [`Source`]: each compile on a reader of its own, let go
//! before it returns; compiled again on the next reader for each write that stops it; what it
//! makes kept for the store; a memo hit with no reader; and a refused compile (a function
//! edited before its first call) failing that function, naming it, and stopping the run.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};

use blaze_cas::{Key, Kind};
use blaze_codegen::driver::Put;
use blaze_codegen::{CompiledFunction, InstanceKey};
use blaze_stencil::tier::Tier;
use parking_lot::Mutex;

use super::hit::{pause, start, target_of};
use super::{compiled, id, key};
use crate::compile::{Kept, Lazily, MOST_STOPS, Refusal, Source};
use crate::runner::Compiler;
use crate::wire::{self, ASK_KIND, FAILED_KIND, Frame, READY_KIND};

/// A database and a lazy build over it: the next `stops` compiles are stopped by a write, each
/// moving it one revision on.
#[derive(Default)]
struct Db {
    revision: AtomicU64,
    /// Readers held now.
    held: AtomicU32,
    stops: AtomicU32,
    /// Instances the front end now describes under another key: edited before their first call.
    moved: Vec<InstanceKey>,
    /// Functions whose template the store has: a compile keeps nothing new.
    stored: Vec<InstanceKey>,
    memo: Vec<InstanceKey>,
    /// Memo templates that can't be made into the function.
    broken: Vec<InstanceKey>,
    /// No reader can be had: the job's front end is gone.
    gone: bool,
    /// Each compile's key, tier and the revision its reader read.
    compiles: Mutex<Vec<(InstanceKey, Option<Tier>, u64)>>,
    kept: Mutex<Vec<Put>>,
    /// The memo hits whose template was kept.
    hits_kept: Mutex<Vec<InstanceKey>>,
}

struct Fake(Arc<Db>);

struct Held {
    db: Arc<Db>,
    revision: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.db.held.fetch_sub(1, SeqCst);
    }
}

fn put(n: u32) -> Put {
    Put { key: Key(id("blaze-lazy test put", n)), kind: Kind::Code, bytes: vec![n as u8] }
}

fn made(at: InstanceKey, puts: u32) -> Kept {
    let mut function = compiled(0, &[], &[]);
    function.key = at;
    Kept { function, puts: (0..puts).map(put).collect() }
}

impl Source for Fake {
    type Reader = Held;

    fn reader(&self) -> Result<Held, String> {
        if self.0.gone {
            return Err("the job's front end was dropped".to_owned());
        }
        self.0.held.fetch_add(1, SeqCst);
        Ok(Held { db: Arc::clone(&self.0), revision: self.0.revision.load(SeqCst) })
    }

    fn memo_hit(&self, at: InstanceKey) -> Option<Result<CompiledFunction, String>> {
        assert_eq!(self.0.held.load(SeqCst), 0, "a memo hit takes no reader");
        if self.0.broken.contains(&at) {
            return Some(Err(format!("{at:?}: the memo's template has 2 places, the function 3")));
        }
        self.0.memo.contains(&at).then(|| Ok(made(at, 0).function))
    }

    fn keep_hit(&self, at: InstanceKey) {
        assert!(self.0.memo.contains(&at), "kept a hit the memo didn't give");
        self.0.hits_kept.lock().push(at);
    }

    fn compile(&self, reader: &Held, at: InstanceKey, to: Option<Tier>) -> Result<Kept, Refusal> {
        assert_eq!(self.0.held.load(SeqCst), 1, "one reader held, this compile's");
        self.0.compiles.lock().push((at, to, reader.revision));
        if self.0.stops.load(SeqCst) > 0 {
            self.0.stops.fetch_sub(1, SeqCst);
            self.0.revision.fetch_add(1, SeqCst);
            return Err(Refusal::Stopped);
        }
        if self.0.moved.contains(&at) {
            return Err(Refusal::Failed(format!(
                "{at:?} changed before its first call: the front end now describes it as {:?}",
                key(99)
            )));
        }
        Ok(made(at, if self.0.stored.contains(&at) { 0 } else { 2 }))
    }

    fn keep(&self, puts: Vec<Put>) {
        self.0.kept.lock().extend(puts);
    }
}

fn lazily(db: Db) -> (Lazily<Fake>, Arc<Db>) {
    let db = Arc::new(db);
    (Lazily::new(Fake(Arc::clone(&db))), db)
}

#[test]
fn a_compile_reads_its_own_reader_and_lets_it_go_before_it_returns() {
    let (lazily, db) = lazily(Db::default());
    let function = lazily.compile(key(3)).unwrap();
    assert_eq!(function.key, key(3));
    assert_eq!(db.held.load(SeqCst), 0, "no reader held between compiles");
    assert_eq!(*db.compiles.lock(), vec![(key(3), None, 0)]);
    assert_eq!(*db.kept.lock(), vec![put(0), put(1)], "the template and side table kept");
}

#[test]
fn each_write_that_stops_a_compile_has_it_compiled_again_on_the_next_reader() {
    let (lazily, db) = lazily(Db { stops: AtomicU32::new(3), ..Db::default() });
    assert_eq!(lazily.compile(key(1)).unwrap().key, key(1));
    let revisions: Vec<u64> = db.compiles.lock().iter().map(|it| it.2).collect();
    assert_eq!(revisions, vec![0, 1, 2, 3], "each attempt on a reader of the revision after the write");
    assert_eq!(db.held.load(SeqCst), 0);
    assert_eq!(db.kept.lock().len(), 2, "kept once, by the compile that ended");
}

#[test]
fn a_compile_stopped_by_too_many_writes_in_a_row_is_an_error_naming_why() {
    let (lazily, db) = lazily(Db { stops: AtomicU32::new(MOST_STOPS), ..Db::default() });
    let why = lazily.compile(key(1)).unwrap_err();
    assert!(why.contains("writes to the database in a row"), "{why}");
    assert_eq!(db.compiles.lock().len(), MOST_STOPS as usize);
    assert!(db.kept.lock().is_empty());
}

#[test]
fn a_refused_compile_is_its_error_and_keeps_nothing() {
    let (lazily, db) = lazily(Db { moved: vec![key(2)], ..Db::default() });
    let why = lazily.compile(key(2)).unwrap_err();
    assert!(why.contains("changed before its first call"), "{why}");
    assert!(db.kept.lock().is_empty());
    assert_eq!(db.held.load(SeqCst), 0);
}

#[test]
fn a_compile_with_no_reader_to_be_had_fails_naming_why() {
    let (lazily, db) = lazily(Db { gone: true, ..Db::default() });
    assert_eq!(lazily.compile(key(0)).unwrap_err(), "the job's front end was dropped");
    assert!(db.compiles.lock().is_empty());
}

#[test]
fn a_template_the_store_had_keeps_nothing() {
    let (lazily, db) = lazily(Db { stored: vec![key(4)], ..Db::default() });
    assert_eq!(lazily.compile(key(4)).unwrap().key, key(4));
    assert!(db.kept.lock().is_empty());
}

#[test]
fn a_memo_hit_takes_no_reader_and_keeps_its_template_once_answered() {
    let (lazily, db) = lazily(Db { memo: vec![key(5)], broken: vec![key(6)], ..Db::default() });
    assert_eq!(lazily.hit(key(5)).unwrap().unwrap().key, key(5));
    assert!(db.hits_kept.lock().is_empty(), "nothing kept before the askers are answered");
    lazily.hit_answered(key(5));
    assert_eq!(*db.hits_kept.lock(), vec![key(5)]);
    assert!(lazily.hit(key(7)).is_none(), "not in the memo");
    let why = lazily.hit(key(6)).unwrap().unwrap_err();
    assert!(why.contains("the memo's template"), "{why}");
    assert!(db.compiles.lock().is_empty(), "a probe compiles nothing");
}

#[test]
fn a_first_compile_takes_a_memo_hit_before_any_reader() {
    let (lazily, db) = lazily(Db { memo: vec![key(5)], broken: vec![key(6)], ..Db::default() });
    assert_eq!(lazily.compile(key(5)).unwrap().key, key(5));
    assert_eq!(*db.hits_kept.lock(), vec![key(5)]);
    assert!(lazily.compile(key(6)).unwrap_err().contains("the memo's template"));
    assert!(db.compiles.lock().is_empty(), "no reader, no compile");
    assert!(db.kept.lock().is_empty());
}

#[test]
fn a_tier_up_compiles_at_its_tier_under_the_same_rule() {
    let (lazily, db) = lazily(Db { stops: AtomicU32::new(1), moved: vec![key(9)], ..Db::default() });
    assert_eq!(lazily.tier_up(key(8), Tier::Cranelift).unwrap().key, key(8));
    assert_eq!(*db.compiles.lock(), vec![(key(8), Some(Tier::Cranelift), 0), (key(8), Some(Tier::Cranelift), 1)]);
    assert!(lazily.tier_up(key(9), Tier::Llvm).unwrap_err().contains("changed before its first call"));
}

#[test]
fn in_a_run_an_edited_function_fails_on_its_first_call_and_stops_the_run() {
    let (lazily, db) = lazily(Db { moved: vec![key(1)], memo: vec![key(2)], broken: vec![key(3)], ..Db::default() });
    let (runner, mail) = start(4, lazily, 1);

    runner.frame(0, ASK_KIND, 1, &wire::ask(1, 0)).unwrap();
    let (kind, body) = mail.wait(0, 1);
    assert_eq!(kind, READY_KIND);
    assert_eq!(Frame::decode(&body).unwrap(), Frame::Ready { request: 1, lazy: 0, target: target_of(0) });

    pause();
    runner.frame(0, ASK_KIND, 2, &wire::ask(2, 2)).unwrap();
    assert!(mail.has(0, 2), "a memo hit answered on the reading thread");
    assert_eq!(mail.wait(0, 2).0, READY_KIND);
    assert_eq!(*db.hits_kept.lock(), vec![key(2)], "its template kept after the answer");

    pause();
    runner.frame(0, ASK_KIND, 3, &wire::ask(3, 3)).unwrap();
    let (kind, body) = mail.wait(0, 3);
    assert_eq!(kind, FAILED_KIND);
    let Frame::Failed { lazy, why, .. } = Frame::decode(&body).unwrap() else { panic!("not a failure") };
    assert_eq!(lazy, 3);
    assert!(why.contains("the memo's template") && why.contains("on its first call"), "{why}");

    runner.frame(0, ASK_KIND, 4, &wire::ask(4, 1)).unwrap();
    let (kind, body) = mail.wait(0, 4);
    assert_eq!(kind, FAILED_KIND);
    let Frame::Failed { lazy, why, .. } = Frame::decode(&body).unwrap() else { panic!("not a failure") };
    assert_eq!(lazy, 1);
    assert!(why.contains("changed before its first call"), "{why}");

    let stops = mail.stops();
    assert_eq!(stops.len(), 2, "each failed function the run called stops it once");
    let closed = runner.close();
    assert_eq!(closed.failures.len(), 2);
    let compiled: Vec<InstanceKey> = db.compiles.lock().iter().map(|it| it.0).collect();
    assert_eq!(compiled, vec![key(0), key(1)], "the hits compiled nothing");
    assert_eq!(*db.hits_kept.lock(), vec![key(2)], "a failed hit keeps nothing");
    assert_eq!(db.held.load(SeqCst), 0);
}
