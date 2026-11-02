//! Dependencies' constants by content (docs/v2/plan/lanes/frontend/fast-const-deps.md): a
//! dependency's `const` item with no generic arguments, once evaluated, is kept in the driver's
//! store under its crate's check-result key and its stable item key ([`key`]), as bytes with no
//! database id ([`codec`]). [`cached`] gives `const_eval_query` the kept value before it reads
//! anything of the body; [`record`] notes a value evaluated on a miss, and the driver writes the
//! values noted with [`drain`] once the build is done.
//!
//! The value of such a constant is the same in every crate that asks for it, whatever
//! `trait_env` it is asked with: with no generic arguments, every trait reference its evaluation
//! selects an impl for is made of types and traits its own crate names, and the orphan rule puts
//! every impl that could apply in that crate or one it depends on, so no dependent can add one.
//! rustc evaluates it once, in its crate, and every dependent reads the value from `.rmeta`.

mod codec;
#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Domain, Hasher};
use crossbeam_queue::SegQueue;
use hir_def::{
    ConstId, HasModule,
    deps::{Held as _, ImageStore, Mode, crate_name},
    nameres::frozen,
};

pub use self::codec::VERSION;
use crate::{
    consteval::ConstEvalError,
    db::GeneralConstId,
    mir::MirEvalError,
    next_solver::{Allocation, GenericArgs, StoredAllocation},
};

/// The domain of a kept value's key.
pub const KEY_DOMAIN: &str = "blaze v2 dependency constant value 1";

static DOMAIN: Domain = Domain::new(KEY_DOMAIN);

/// What [`frozen::record_difference`] names the differences [`Mode::Check`] and
/// [`Mode::Verify`] find.
const KIND: &str = "constant values";

/// The key the value of the constant whose stable key is `item` (`hir_def::deps::const_key`)
/// is kept under, in the crate whose check results are filed under `base`: what it is made from,
/// and the version of its bytes.
pub fn key(base: ContentId, item: ContentId) -> ContentId {
    let mut hasher = Hasher::of(&DOMAIN);
    hasher.id(&base).id(&item).update(&[VERSION]);
    hasher.finish()
}

/// Where the values of dependencies' constants are kept, and which crates' are: set by the
/// driver ([`set`]).
#[blaze_query::input(singleton, debug)]
pub struct KeptConsts {
    #[returns(ref)]
    pub kept: Kept,
}

/// The store, the mode, the crates whose constants are kept, and the values noted to be
/// written. Compares by identity: [`set`] gives a new one only when something in it changed.
#[derive(Clone)]
pub struct Kept(Arc<Shared>);

struct Shared {
    // dyn: the driver's store, one probe per constant of a dependency per session
    store: Arc<dyn ImageStore + Send + Sync>,
    mode: Mode,
    /// Each crate whose constants are kept, with the key its check results are filed under
    /// when that is known now: `None` for a crate checked in this session, whose key is known
    /// only once its bodies are, at [`drain`].
    crates: HashMap<Crate, Option<ContentId>>,
    pending: SegQueue<Pending>,
}

/// A value [`record`] noted, for [`drain`] to file and write.
struct Pending {
    krate: Crate,
    def: ConstId,
    item: ContentId,
    body: Vec<u8>,
}

impl PartialEq for Kept {
    fn eq(&self, other: &Kept) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Kept {}

impl std::hash::Hash for Kept {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kept")
            .field("mode", &self.0.mode)
            .field("crates", &self.0.crates.len())
            .finish_non_exhaustive()
    }
}

/// Has `db` read and keep the constants of `crates` in `store`, doing `mode` with them
/// ([`Mode::Serve`] serves a kept value, [`Mode::Check`] evaluates and records a difference from
/// the kept value, [`Mode::Verify`] serves and records a difference between the value written
/// again and the kept bytes). `crates` are dependencies (`hir_def::deps::eligible`) whose check
/// results are filed by frontend-fast-bodies-cache, each with the key they are filed under when
/// it is known now.
///
/// Values noted and not yet drained are dropped with the input they were noted in: drain before
/// setting again. The same store, mode and crates as now changes nothing, so a constant served
/// before is not asked again.
pub fn set(
    db: &mut Db,
    // dyn: the driver's store, as `Shared::store`
    store: Arc<dyn ImageStore + Send + Sync>,
    crates: HashMap<Crate, Option<ContentId>>,
    mode: Mode,
) {
    use blaze_query::db::{Durability, Setter};
    let current = KeptConsts::try_get(&*db);
    if let Some(it) = current {
        let now = &it.kept(&*db).0;
        if Arc::ptr_eq(&now.store, &store) && now.mode == mode && now.crates == crates {
            return;
        }
    }
    let pending = SegQueue::new();
    // alloc: setup: the one input value, once per session
    let kept = Kept(Arc::new(Shared { store, mode, crates, pending }));
    match current {
        Some(it) => {
            it.set_kept(db).with_durability(Durability::HIGH).to(kept);
        }
        None => {
            _ = KeptConsts::builder(kept).durability(Durability::HIGH).new(db);
        }
    }
}

/// A constant whose value is kept: its crate's entry and its item's key.
struct Site<'a> {
    shared: &'a Shared,
    krate: Crate,
    item: ContentId,
    base: Option<ContentId>,
}

/// Where the value of `def` for `subst` is kept, if it is one that is: no generic arguments,
/// a crate the driver keeps the constants of, and not in a block. A workspace crate's constant
/// reads nothing more than its crate's origin, so setting the store changes none of them.
fn site<'a>(db: &'a Db, def: ConstId, subst: GenericArgs<'_>) -> Option<Site<'a>> {
    if !subst.is_empty() {
        return None;
    }
    let krate = def.krate(db);
    if krate.data(db).origin.is_local() {
        return None;
    }
    let shared = &*KeptConsts::try_get(db)?.kept(db).0;
    let &base = shared.crates.get(&krate)?;
    let item = hir_def::deps::const_key(db, def)?;
    Some(Site { shared, krate, item, base })
}

/// The kept value of `def`, a dependency's `const` item, for `subst`, when the store has it:
/// `Ok(None)` for a constant that is not kept (not a dependency's, generic arguments, no store
/// given to the database) or not found; `Err` when the kept bytes don't read back, naming it.
pub(crate) fn cached<'db>(
    db: &'db Db,
    def: ConstId,
    subst: GenericArgs<'db>,
) -> Result<Option<Allocation<'db>>, ConstEvalError<'db>> {
    let Some(site) = site(db, def, subst) else { return Ok(None) };
    let (Some(base), Mode::Serve | Mode::Verify) = (site.base, site.shared.mode) else {
        return Ok(None);
    };
    let key = key(base, site.item);
    let kept = match site.shared.store.get(&key) {
        Ok(Some(kept)) => kept,
        Ok(None) => return Ok(None),
        Err(why) => return Err(unreadable(db, def, site.krate, key, &why)),
    };
    let refused = |why: &str| unreadable(db, def, site.krate, key, why);
    let body = codec::body(kept.bytes(), key).map_err(refused)?;
    let value = codec::decode_body(body).map_err(refused)?;
    if site.shared.mode == Mode::Verify {
        let written = codec::encode(value);
        compare(db, def, site.krate, "what was read of it", written.as_deref(), body);
    }
    Ok(Some(value))
}

/// Records the value `def` was evaluated to for the driver to write to the store, when it is
/// one [`cached`] would serve.
pub(crate) fn record<'db>(
    db: &'db Db,
    def: ConstId,
    subst: GenericArgs<'db>,
    value: &Result<StoredAllocation, ConstEvalError<'db>>,
) {
    let Ok(value) = value else { return };
    let Some(site) = site(db, def, subst) else { return };
    let Some(body) = codec::encode(value.as_ref()) else { return };
    if let (Mode::Check, Some(base)) = (site.shared.mode, site.base) {
        let key = key(base, site.item);
        match site.shared.store.get(&key) {
            Ok(None) => {}
            Ok(Some(kept)) => {
                match codec::body(kept.bytes(), key) {
                    Ok(kept) => {
                        compare(db, def, site.krate, "the value evaluated", Some(&body[..]), kept);
                    }
                    Err(why) => {
                        // alloc: a difference, which is a bug
                        let what = format!("the value evaluated (the kept bytes do not read: {why})");
                        compare(db, def, site.krate, &what, Some(&body[..]), &[]);
                    }
                }
                return;
            }
            Err(why) => {
                // alloc: a difference, which is a bug
                let what = format!("the value evaluated (the store could not be read: {why})");
                compare(db, def, site.krate, &what, Some(&body[..]), &[]);
                return;
            }
        }
    }
    site.shared.pending.push(Pending { krate: site.krate, def, item: site.item, body });
}

/// Records a difference between `written`, what `what` wrote of `def` (`None` when it wrote
/// nothing), and `kept`, the kept body.
fn compare(db: &Db, def: ConstId, krate: Crate, what: &str, written: Option<&[u8]>, kept: &[u8]) {
    match written {
        Some(written) if written == kept => {}
        Some(written) => {
            // alloc: a difference, which is a bug
            let what = format!("{what}, of constant `{}`", GeneralConstId::ConstId(def).name(db));
            frozen::record_difference(db, krate, KIND, &what, written, kept);
        }
        None => {
            let name = GeneralConstId::ConstId(def).name(db);
            // alloc: a difference, which is a bug
            let what = format!("{what}, of constant `{name}`, which does not write");
            frozen::record_difference(db, krate, KIND, &what, &[], kept);
        }
    }
}

/// The error of a kept value of `def` that does not read: the store checked its bytes against
/// their hash, so it is a bug in the key or the encoding, and evaluating instead would hide it.
#[cold]
#[inline(never)]
fn unreadable<'db>(
    db: &'db Db,
    def: ConstId,
    krate: Crate,
    key: ContentId,
    why: &str,
) -> ConstEvalError<'db> {
    let name = GeneralConstId::ConstId(def).name(db);
    // alloc: error text
    let said = format!(
        "the kept value of constant `{name}` of crate {} (key {key:?}) does not read: {why}",
        crate_name(db, krate)
    );
    ConstEvalError::MirEvalError(MirEvalError::InternalError(said.into_boxed_str()))
}

/// What [`drain`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    /// The values put to the store.
    pub written: usize,
    /// The values of crates `bases` has no key for, which are not kept: the check results of
    /// such a crate are refused, so its constants are too.
    pub unkeyed: usize,
}

/// Writes to the store [`set`] gave each value [`record`] noted since, filed under [`key`] with
/// the key `bases` gives its crate (every crate whose check results are kept, those served and
/// those checked in this session), on the store's threads ([`ImageStore::for_each`]). Run by the
/// driver after the build, never by a worker.
///
/// A value the store will not keep, and two values of one key (an evaluation that is not a
/// function of what its key covers), are errors naming the constant.
pub fn drain(db: &Db, bases: &HashMap<Crate, ContentId>) -> Result<Drained, String> {
    let Some(kept) = KeptConsts::try_get(db) else { return Ok(Drained::default()) };
    let shared = &*kept.kept(db).0;
    let mut drained = Drained::default();
    // alloc: setup: the values of one session, once
    let mut filed: Vec<(ContentId, Pending)> = Vec::new();
    while let Some(pending) = shared.pending.pop() {
        match bases.get(&pending.krate) {
            Some(&base) => filed.push((key(base, pending.item), pending)),
            None => drained.unkeyed += 1,
        }
    }
    filed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    filed.dedup_by(|later, kept| later.0 == kept.0 && later.1.body == kept.1.body);
    if let Some(pair) = filed.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        let (key, pending) = &pair[0];
        // alloc: error text
        return Err(format!(
            "constant `{}` of crate {} was evaluated to two values (key {key:?}): its value is \
             not a function of what its key covers",
            GeneralConstId::ConstId(pending.def).name(db),
            crate_name(db, pending.krate)
        ));
    }
    let done: Vec<OnceLock<Result<(), String>>> =
        // alloc: setup: one slot per value, once per session
        filed.iter().map(|_| OnceLock::new()).collect();
    shared.store.for_each(filed.len(), &|at| {
        let (key, pending) = &filed[at];
        _ = done[at].set(shared.store.put(*key, codec::file(*key, &pending.body)));
    });
    for ((key, pending), done) in filed.iter().zip(done) {
        let done = done.into_inner().unwrap_or_else(|| {
            panic!("`ImageStore::for_each` did not run the task of a constant value")
        });
        done.map_err(|why| {
            // alloc: error text
            format!(
                "keeping the value of constant `{}` of crate {} (key {key:?}): {why}",
                GeneralConstId::ConstId(pending.def).name(db),
                crate_name(db, pending.krate)
            )
        })?;
    }
    drained.written = filed.len();
    Ok(drained)
}
