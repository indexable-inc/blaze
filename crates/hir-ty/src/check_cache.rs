//! A dependency's bodies, checked once for everyone
//! (docs/v2/plan/design/frontend.md): for each body of a dependency
//! crate, what codegen reads of its check, its inference result and whether its borrow check
//! passed, kept in the store beside frontend-deps' image as one part per crate ([`encode`]) and
//! given to another process's database ([`set_served`]). `infer_body_query` returns a served
//! body's result ([`served_inference`]) and `hir_ty_borrowck::gate::checked` takes a served
//! verdict ([`served_passed`]). MIR is not kept: a consumer lowers it from the served result.
//!
//! Every id the record writes goes through a table ([`ids`]), so the bytes name no database's own id.
//! Inference reads no lint level, and the verdict is whether the borrow check found an error,
//! which lint levels do not change: the part's [`key`] is the image's and the borrow checker's
//! mode.

pub(crate) mod ids;
pub(crate) mod pages;
mod part;
pub mod port;
pub mod runtime;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use base_db::{Crate, Db, Given};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Domain, Hasher};
use hir_def::{
    DefWithBodyId, HasModule,
    deps::{Held as _, ImageBuf, body_key},
};
use std::alloc::Global;

pub(crate) use self::ids::{IdRefused, Relocate, id_bits, id_of, in_scope, outside};
use self::{
    ids::IdKind,
    part::{BodyRef, PartRef},
};
use crate::InferenceResult;

/// The domain of a bodies part's key.
pub const BODIES_DOMAIN: &str = "blaze v2 dependency bodies 1";

static DOMAIN: Domain = Domain::new(BODIES_DOMAIN);

/// What makes the parts: the schema of their bytes, which covers every encoding in them, so a
/// part of another encoding is never found.
#[expect(
    clippy::disallowed_types,
    reason = "the process's one producer content ID: all body parts use the same schema"
)]
pub static PRODUCER: std::sync::LazyLock<ContentId> = std::sync::LazyLock::new(|| {
    let mut hasher = Hasher::of(&DOMAIN);
    hasher.part(b"producer").id(&part::SCHEMA.0);
    hasher.finish()
});

/// The key the bodies part of a crate is filed under: its image's key as filed
/// (`hir_def::deps::Image::key`, which covers what the crate's check reads of the machine) and
/// the borrow checker's mode (`hir_ty_borrowck::mode`, as its `u8`), with [`PRODUCER`].
pub fn key(image: ContentId, mode: u8) -> ContentId {
    let mut hasher = Hasher::of(&DOMAIN);
    hasher.id(&PRODUCER).id(&image).update(&[mode]);
    hasher.finish()
}

/// The bodies part of `krate`, from its bodies as this database inferred them; `passed` gives
/// each one's borrow check verdict. A body with no portable form is left out of the part (its
/// consumer infers it); the part is refused only when its items do not encode or two of its
/// bodies have one key.
pub fn encode(
    db: &Db,
    krate: Crate,
    // dyn: the driver's verdicts, one call per body at store time
    passed: &(dyn Fn(DefWithBodyId) -> bool + Sync),
) -> Result<Vec<u8>, String> {
    part::encode(db, krate, passed, &|_| None)
}

/// Whether `bytes` are a bodies part: its header, schema and columns, checked in place, as
/// [`set_served`] needs them. A body's own bytes are checked when it is read.
pub fn open(bytes: &[u8]) -> Result<(), String> {
    part::open(bytes).map(|_| ())
}

/// Has `db` serve the bodies of each crate of `parts` from its part, which [`open`] accepted:
/// one that does not open here is a panic naming its crate. Each crate's part is given to that
/// crate ([`base_db::Edit::give`], kind [`BodiesSlot`], all in one commit, so every crate id
/// the caller holds from before is to be taken again by its key after it), never kept in one
/// map of every crate: a map
/// of every crate is a value of the session arena that names every crate's ids, so it would
/// borrow every crate arena and any crate's change would drop the session's (rule 2). A crate
/// given the same buffer as now is not given it again; a crate with a part before and none
/// in `parts` has it taken away.
pub fn set_served(db: &mut Db, parts: HashMap<Crate, ImageBuf, Global>) {
    for (&krate, buf) in &parts {
        if let Err(why) = part::open(buf.bytes()) {
            panic!(
                "the bodies part of crate {} does not open: {why}; `set_served` takes parts \
                 `open` accepted",
                krate.named(db)
            );
        }
    }
    // Each gift by the crate's key, decided in this snapshot and given in one edit: the
    // commit drops each given crate's arena, and every id of it with it (`parts`' keys too).
    let crates = base_db::all_crates(&*db);
    let mut gifts: Vec<(base_db::CrateKey, Given), Global> = Vec::new_in(Global);
    for &krate in crates.iter() {
        let now = krate.given::<BodiesSlot, PartBuf>(&*db);
        let given = match (now, parts.get(&krate)) {
            (None, None) => continue,
            (Some(now), Some(buf)) if Arc::ptr_eq(&now.0, buf) => continue,
            (Some(_), None) => Given::default(),
            // clone: a counted reference to the part's buffer
            (_, Some(buf)) => Given::new(PartBuf(buf.clone())),
        };
        gifts.push((krate.key(&*db), given));
    }
    drop(crates);
    if gifts.is_empty() {
        return;
    }
    let mut edit = db.edit();
    for (key, given) in gifts {
        edit.give::<BodiesSlot>(key, given);
    }
    _ = edit.commit();
}

/// Whether `def`'s body is served and its borrow check passed where it was checked:
/// `hir_ty_borrowck::gate::checked` then runs no borrow check. `false` for a body that is not
/// served, and always under an item store in `Mode::Trace` ([`served_body`]).
pub fn served_passed(db: &Db, def: DefWithBodyId) -> bool {
    served_body(db, def).is_some_and(|(_, body)| body.passed())
}

/// `def`'s inference result, read from its crate's part, `None` when the body is not served (its
/// crate has no part, it is in a block, or it was left out of the part). A record that does not
/// read is a panic naming the crate and body: the store checked the bytes against their key, so
/// inferring the body instead would hide a bug.
pub fn served_inference(db: &Db, def: DefWithBodyId) -> Option<InferenceResult<'_>> {
    let (krate, body) = served_body(db, def)?;
    let items = served_items(db, krate);
    // The record's nodes are checked against the counts of the body's store its writer kept
    // beside it (`BodyRef::nodes`): the part's key covers the crate's sources, so they are
    // the store's here, and a hit lowers no body (it did, for `Nodes::of`: one `body_query`
    // a served body, `a_served_body_is_read_without_lowering_it`).
    let nodes = body.nodes();
    let result = body
        .inference(db, krate, items, nodes)
        .unwrap_or_else(|why| corrupt(db, krate, Some(def), &why));
    Some(result)
}

/// The kind of a crate's slot holding its bodies part ([`set_served`]).
pub(crate) struct BodiesSlot;

/// One crate's part, compared by the buffer it is: [`set_served`] gives a crate a part again
/// only when its buffer is another.
#[derive(Clone)]
struct PartBuf(ImageBuf);

impl PartialEq for PartBuf {
    fn eq(&self, other: &PartBuf) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for PartBuf {}

impl std::fmt::Debug for PartBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartBuf").field("len", &self.0.bytes().len()).finish()
    }
}

/// The crate's items, made in this database once per crate (kept in the crate's arena): what
/// each body's ids name.
#[blaze_memo::tracked(returns(copy), mem_tag = Infer)]
fn served_items<'db>(db: &'db Db, krate: Crate) -> &'db [(IdKind, u64)] {
    let part = view(db, krate).expect("the items of a crate with a part");
    let items = part.items(db, krate).unwrap_or_else(|why| corrupt(db, krate, None, &why));
    blaze_memo::keep_slice(blaze_memo::Database::memo(db), &items)
}

/// The part of `krate`, opened in place, `None` when it has none. Reads the crate's slot alone
/// (`Crate::given`): the read a body's inference makes of the served parts.
fn view(db: &Db, krate: Crate) -> Option<PartRef<'_>> {
    let buf = krate.given::<BodiesSlot, PartBuf>(db)?;
    Some(part::view(buf.0.bytes()).unwrap_or_else(|why| corrupt(db, krate, None, &why)))
}

/// `def`'s crate and its body's buffer, when it is served: `None` when its crate has no part, it
/// is in a block (it has no key) or the part lists it as left out. A body of a served crate whose
/// key the part neither records nor lists is a panic naming it: the writing database gave its
/// bodies other keys, and inferring it instead would hide that.
///
/// Never under an item store in `Mode::Trace` (`body_store::tracing`): there no step's answer
/// comes from anywhere but its own run, so neither [`served_inference`] nor [`served_passed`]
/// reads a part, whatever parts the database was given. The one door to both, so neither can.
fn served_body(db: &Db, def: DefWithBodyId) -> Option<(Crate, BodyRef<'_>)> {
    // Nor under `Mode::Given`, where a body's answers are its body record's alone.
    if crate::body_store::tracing(db) || crate::body_store::given(db) {
        return None;
    }
    let krate = def.krate(db);
    let part = view(db, krate)?;
    let key = body_key(db, def)?;
    let Some(bytes) = part.find(&key) else {
        if part.left_out(&key).is_some() {
            return None;
        }
        corrupt(
            db,
            krate,
            Some(def),
            "its key is neither recorded nor left out: the database that wrote the part gave its \
             bodies other keys",
        )
    };
    let body = part::open_body(bytes).unwrap_or_else(|why| corrupt(db, krate, Some(def), &why));
    Some((krate, body))
}

#[cold]
#[inline(never)]
fn corrupt(db: &Db, krate: Crate, def: Option<DefWithBodyId>, why: &str) -> ! {
    let what = match def {
        // alloc: the panic's message
        Some(def) => format!(", the body of {def:?}"),
        // alloc: none; an empty `String` holds no allocation
        None => String::new(),
    };
    panic!(
        "the bodies part of crate {} is corrupt{what}: {why}; its bytes were checked against \
         their key, so the key or the encoding is wrong, and the body is not inferred instead",
        krate.named(db)
    )
}
