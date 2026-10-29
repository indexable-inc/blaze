//! The compiles under way and the slow ones done: what the terminal shows under each crate
//! compiling (its functions compiling longest) and at the end (those that took longest).
//!
//! Nothing of it runs until [`begin`] is told someone hears the build.

use std::{
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base_db::Crate;
use hir_def::{FunctionId, HasModule, signatures::FunctionSignature};
use hir_ty::db::HirDatabase;
use rustc_hash::FxHashMap;

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
static LIVE: LazyLock<Mutex<FxHashMap<u64, (Crate, String, Instant)>>> =
    LazyLock::new(Default::default);

/// The compiles done since [`begin`] that took [`SLOW`] or more.
static DONE: Mutex<Vec<Flown>> = Mutex::new(Vec::new());

fn lock<T>(it: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    it.lock().unwrap_or_else(|e| e.into_inner())
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
    let live = lock(&LIVE);
    let mut flown: Vec<Flown> = live
        .values()
        .filter(|it| now - it.2 >= longer)
        .map(|(krate, name, since)| Flown { krate: *krate, name: name.clone(), took: now - *since })
        .collect();
    flown.sort_by_key(|it| std::cmp::Reverse(it.took));
    flown
}

/// The compiles done since [`begin`] that took [`SLOW`] or more, longest first.
pub fn slowest() -> Vec<Flown> {
    let mut done = std::mem::take(&mut *lock(&DONE));
    done.sort_by_key(|it| std::cmp::Reverse(it.took));
    done
}

/// A compile, from now until it is dropped, while someone hears the build.
pub(crate) struct Flight(Option<u64>);

impl Flight {
    pub(crate) fn root(db: &dyn HirDatabase, func: FunctionId) -> Flight {
        Flight::of(|| (func.krate(db), root_name(db, func).clone()))
    }

    pub(crate) fn instance(
        db: &dyn HirDatabase,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
    ) -> Flight {
        Flight::of(|| instance_named(db, instance.clone(), env.clone()).clone())
    }

    fn of(what: impl FnOnce() -> (Crate, String)) -> Flight {
        if !ON.load(Ordering::Relaxed) {
            return Flight(None);
        }
        let (krate, name) = what();
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        lock(&LIVE).insert(id, (krate, name, Instant::now()));
        Flight(Some(id))
    }
}

// The compile that names itself depends on the name: a query of its own keeps what the name is
// spelled from (bodies, for one) out of the compile's inputs, so an edit that leaves the name
// as it was recompiles nothing.
#[salsa::tracked(returns(ref))]
fn root_name(db: &dyn HirDatabase, func: FunctionId) -> String {
    let name = hir_ty_mir::mir::type_name::rustc_function_name(db, func, &[]);
    name.unwrap_or_else(|| FunctionSignature::of(db, func).name.as_str().to_owned())
}

#[salsa::tracked(returns(ref))]
fn instance_named(
    db: &dyn HirDatabase,
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
        let Some((krate, name, since)) = lock(&LIVE).remove(&id) else { return };
        let took = since.elapsed();
        if took >= SLOW {
            let mut done = lock(&DONE);
            if done.len() < KEPT {
                done.push(Flown { krate, name, took });
            } else if let Some(least) = done.iter_mut().min_by_key(|it| it.took)
                && least.took < took
            {
                *least = Flown { krate, name, took };
            }
        }
    }
}
