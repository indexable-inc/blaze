//! The compiles under way and the slow ones done: what the terminal shows under each crate
//! compiling (its functions compiling longest) and at the end (those that took longest).
//!
//! Nothing of it runs until [`begin`] is told someone hears the build.

#![forbid(unsafe_code)]

use std::{
    sync::{
        LazyLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base_db::{Crate, Db};
use blaze_hash::SharedMap;
use hir_def::{FunctionId, HasModule, signatures::FunctionSignature};
use parking_lot::Mutex;

use crate::{Instance, InstanceDef, StoredParamEnvAndCrate};

/// A compile that took this long is one of the build's slow ones.
pub const SLOW: Duration = Duration::from_millis(200);

/// The most slow compiles a build keeps: the end lists them longest first.
const KEPT: usize = 256;

static ON: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A function compiling or compiled: of which crate, its name, and how long.
pub struct Flown {
    pub krate: Crate,
    pub name: String,
    pub took: Duration,
}

/// The compiles under way, by the number each took.
static LIVE: LazyLock<SharedMap<u64, (Crate, String, Instant)>> = LazyLock::new(Default::default);

/// The compiles done since [`begin`] that took [`SLOW`] or more: the [`KEPT`] slowest, a lock
/// only a compile that slow takes.
// alloc: empty, allocates nothing until a compile is slow
static DONE: Mutex<Vec<Flown>> = Mutex::new(Vec::new());

fn lock<T>(it: &Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    it.lock()
}

/// A build begins: its compiles are kept track of when `heard`, and the slow ones of the
/// builds before are forgotten.
pub fn begin(heard: bool) {
    ON.store(heard, Ordering::Relaxed);
    lock(&DONE).clear();
}

/// The compiles under way for `longer` than that, longest first.
pub fn under_way(longer: Duration) -> Vec<Flown> {
    let now = Instant::now();
    let live = LIVE.pin();
    let mut flown: Vec<Flown> = live
        .values()
        .filter(|it| now - it.2 >= longer)
        // clone: the name, out of the lock, for a terminal refresh
        .map(|(krate, name, since)| Flown { krate: *krate, name: name.clone(), took: now - *since })
        // alloc: a terminal refresh's few slow compiles
        .collect();
    blaze_ids::radix::sort_by_key(&mut flown, |it| !it.took.as_nanos());
    flown
}

/// The compiles done since [`begin`] that took [`SLOW`] or more, longest first.
pub fn slowest() -> Vec<Flown> {
    let mut done = std::mem::take(&mut *lock(&DONE));
    blaze_ids::radix::sort_by_key(&mut done, |it| !it.took.as_nanos());
    done
}

/// A compile, from now until it is dropped, while someone hears the build.
pub(crate) struct Flight(Option<u64>);

impl Flight {
    pub(crate) fn root(db: &Db, func: FunctionId) -> Flight {
        // clone: only while someone hears the build; the live table owns the name
        Flight::of(|| (func.krate(db), root_name(db, func).clone()))
    }

    pub(crate) fn instance(
        db: &Db,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
    ) -> Flight {
        // clone: only while someone hears the build: the query's keys and the live table's name
        Flight::of(|| instance_named(db, instance.clone(), env.clone()).clone())
    }

    fn of(what: impl FnOnce() -> (Crate, String)) -> Flight {
        if !ON.load(Ordering::Relaxed) {
            return Flight(None);
        }
        let (krate, name) = what();
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        LIVE.pin().insert(id, (krate, name, Instant::now()));
        Flight(Some(id))
    }
}

// The compile that names itself depends on the name: a query of its own keeps what the name is
// spelled from (bodies, for one) out of the compile's inputs, so an edit that leaves the name
// as it was recompiles nothing.
#[blaze_query::tracked(returns(ref))]
fn root_name(db: &Db, func: FunctionId) -> String {
    let name = hir_ty_mir::mir::type_name::rustc_function_name(db, func, &[]);
    // alloc: a memoized query's value, once per function while someone hears the build
    name.unwrap_or_else(|| FunctionSignature::of(db, func).name.as_str().to_owned())
}

#[blaze_query::tracked(returns(ref))]
fn instance_named(
    db: &Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> (Crate, String) {
    let krate = match instance.def {
        InstanceDef::Fn(func) => func.krate(db),
        _ => env.krate,
    };
    (krate, crate::backtrace_name(db, &instance))
}

impl Drop for Flight {
    fn drop(&mut self) {
        let Some(id) = self.0.take() else { return };
        let live = LIVE.pin();
        let Some((krate, name, since)) = live.remove(&id) else { return };
        let took = since.elapsed();
        if took >= SLOW {
            // clone: a slow compile's name, kept for the build's end; the live table drops its own
            let flown = || Flown { krate: *krate, name: name.clone(), took };
            let mut done = lock(&DONE);
            if done.len() < KEPT {
                done.push(flown());
            } else if let Some(least) = done.iter_mut().min_by_key(|it| it.took)
                && least.took < took
            {
                *least = flown();
            }
        }
    }
}
