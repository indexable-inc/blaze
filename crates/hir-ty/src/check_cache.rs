//! A dependency's bodies, checked once for everyone
//! (docs/v2/plan/lanes/frontend/fast-bodies-cache-bodies.md): for each body of a dependency
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
mod part;
#[cfg(test)]
mod tests;

use std::sync::{Arc, LazyLock};

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Domain, Hasher};
use hir_def::{
    DefWithBodyId, HasModule,
    deps::{Held as _, ImageBuf, body_key, crate_name},
};

pub(crate) use self::ids::{Relocate, id_bits, id_of, in_scope, outside};
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
pub static PRODUCER: LazyLock<ContentId> = LazyLock::new(|| {
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
/// one that does not open here is a panic naming its crate. The same parts as now (the same
/// buffers) change nothing.
pub fn set_served(db: &mut Db, parts: HashMap<Crate, ImageBuf>) {
    use blaze_query::db::{Durability, Setter};
    for (&krate, buf) in &parts {
        if let Err(why) = part::open(buf.bytes()) {
            panic!(
                "the bodies part of crate {} does not open: {why}; `set_served` takes parts \
                 `open` accepted",
                crate_name(db, krate)
            );
        }
    }
    let current = ServedBodies::try_get(&*db);
    if let Some(it) = current {
        let now = &it.parts(&*db).0;
        let same = now.len() == parts.len()
            && parts.iter().all(|(krate, buf)| now.get(krate).is_some_and(|it| Arc::ptr_eq(it, buf)));
        if same {
            return;
        }
    }
    // alloc: setup: the one input value, once per session
    let served = Served(Arc::new(parts));
    match current {
        Some(it) => {
            it.set_parts(db).with_durability(Durability::HIGH).to(served);
        }
        None => {
            _ = ServedBodies::builder(served).durability(Durability::HIGH).new(db);
        }
    }
}

/// Whether `def`'s body is served and its borrow check passed where it was checked:
/// `hir_ty_borrowck::gate::checked` then runs no borrow check. `false` for a body that is not
/// served.
pub fn served_passed(db: &Db, def: DefWithBodyId) -> bool {
    served_body(db, def).is_some_and(|(_, body)| body.passed())
}

/// `def`'s inference result, read from its crate's part, `None` when the body is not served (its
/// crate has no part, it is in a block, or it was left out of the part). A record that does not
/// read is a panic naming the crate and body: the store checked the bytes against their key, so
/// inferring the body instead would hide a bug.
pub(crate) fn served_inference(db: &Db, def: DefWithBodyId) -> Option<InferenceResult<'_>> {
    let (krate, body) = served_body(db, def)?;
    let items = served_items(db, krate);
    let result = body
        .inference(db, krate, items)
        .unwrap_or_else(|why| corrupt(db, krate, Some(def), &why));
    Some(result)
}

/// The parts of the crates whose bodies are served, set by [`set_served`].
#[blaze_query::input(singleton, debug)]
pub(crate) struct ServedBodies {
    #[returns(ref)]
    parts: Served,
}

/// The parts, by crate. Compares by identity: [`set_served`] gives a new one only when a part
/// changed.
#[derive(Clone)]
pub(crate) struct Served(Arc<HashMap<Crate, ImageBuf>>);

impl PartialEq for Served {
    fn eq(&self, other: &Served) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Served {}

impl std::hash::Hash for Served {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}

impl std::fmt::Debug for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served").field("crates", &self.0.len()).finish()
    }
}

/// One crate's part, compared by the buffer it is: what a body's inference reads of the served
/// parts, so setting them again leaves the bodies of a crate whose part is the same buffer valid.
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

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn served_part(db: &Db, krate: Crate) -> Option<PartBuf> {
    ServedBodies::try_get(db)?.parts(db).0.get(&krate).cloned().map(PartBuf)
}

/// The crate's items, made in this database once per crate: what each body's ids name.
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
fn served_items(db: &Db, krate: Crate) -> Box<[(IdKind, u64)]> {
    let part = view(db, krate).expect("the items of a crate with a part");
    part.items(db, krate).unwrap_or_else(|why| corrupt(db, krate, None, &why))
}

/// The part of `krate`, opened in place, `None` when it has none.
fn view(db: &Db, krate: Crate) -> Option<PartRef<'_>> {
    let buf = served_part(db, krate).as_ref()?;
    Some(part::view(buf.0.bytes()).unwrap_or_else(|why| corrupt(db, krate, None, &why)))
}

/// `def`'s crate and its body's buffer, when it is served: `None` when its crate has no part, it
/// is in a block (it has no key) or the part lists it as left out. A body of a served crate whose
/// key the part neither records nor lists is a panic naming it: the writing database gave its
/// bodies other keys, and inferring it instead would hide that.
fn served_body(db: &Db, def: DefWithBodyId) -> Option<(Crate, BodyRef<'_>)> {
    ServedBodies::try_get(db)?;
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
        crate_name(db, krate)
    )
}
