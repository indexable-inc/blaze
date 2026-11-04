//! [`super::Hashes`] over the front end's code identity: what the code generator puts in place
//! of the database's dense ids.
//!
//! - A body's code is [`hir_ty_share::code`]'s: its item's code hash, which takes each callee by
//!   its interface hash, with the positions its MIR names (the spans of [`ItemHashes::span`])
//!   beside it: a template that names an expression by its id is never reused once that id is
//!   another expression's. With it the key of the store the body is written in, which its
//!   spans' anchors are of: one body written alike in two crates is two codes, each naming its
//!   own tables, so two instances, each linked with its own positions.
//! - Its template code ([`Hashes::template_code`]) is the same without the store's key: no
//!   name and no place is in it. What the compiled body reads of names beside its code is
//!   [`Hashes::names_read`]: the files its `file!()` names and, where a debugger is told of
//!   its locals, what their types' identities read (`code::told_names`). The front end folds
//!   that into both its code and its template code. A template keyed by it holds its own
//!   anchors by table, and each instance fills its own (docs/v2/plan/design/name-free-identity.md).
//! - A type or constant is [`hir_ty_share::identity`]'s spelling by content, the one the share
//!   key reads too: an ADT by its key, its interface, its layout and what drop glue reads of
//!   it, a coroutine by its compiled code, and its arguments the same way. Regions are left out.
//! - A position is a [`SpanRef`] into one of the tables of the body's owner, one per kind of
//!   span ([`hir_ty_share::code::table_of`]), indexed by the span's id in the owner's store: an
//!   edit elsewhere in the file moves no index, and a formatting edit changes the table only
//!   ([`ItemHashes::span_tables`]).
//!
//! Nothing here is shared between threads but the query database's memos: no lock on any path.

#[cfg(test)]
mod tests;

use base_db::Db;
use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemId, ItemKey, item_key, key};
use blaze_query::db::FromId;
use hir_def::{DefWithBodyId, ExpressionStoreOwnerId, FunctionId, expr_store::Body as BodyStore};
use hir_ty::{
    InferBodyId,
    db::{HirDatabase as _, InternedClosureId, InternedCoroutineId},
    next_solver::{Const, Ty},
};
use hir_ty_mir::mir::MirSpan;
use hir_ty_share::{
    code::{self, TABLES},
    identity,
};

use super::{Body, Hashes};

pub const SPAN_DOMAIN: &str = "blaze v2 codegen span table 1";
pub const OWNED_DOMAIN: &str = "blaze v2 codegen owned code 1";
pub const TEMPLATE_CODE_DOMAIN: &str = "blaze v2 codegen template code 2";
pub const NAMES_READ_DOMAIN: &str = "blaze v2 codegen names read 1";

// A template holds an instance's own positions by table, one anchor per table of the body's.
const _: () = assert!(TABLES == blaze_codegen::template::OWN_TABLES);

/// The table of a function's declaration, apart from its body's.
const DECLARATION: u8 = TABLES as u8;

/// The items lane's hashes as the code generator asks for them, and what the program calls
/// each file.
pub struct ItemHashes {
    // dyn: blaze-cargo's naming of files, set once per build; asked per `file!()` and position, each allocating its name
    names: Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>,
}

impl ItemHashes {
    /// `names` says what the program calls a file (`file!()`): its path relative to its
    /// package's root, `None` for a file the build did not name.
    // dyn: blaze-cargo's naming of files, set once per build; asked per `file!()` and position, each allocating its name
    pub fn new(names: Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>) -> ItemHashes {
        ItemHashes { names }
    }

    /// Every span table of `owner`'s body that [`Hashes::span`] names, by its anchor: the span
    /// at each index, for the linker's table to give the position of
    /// (`hir_ty_mir::mir::location::source_location_in_file`). A span that function finds no syntax for
    /// (a binding, the self parameter, [`MirSpan::Unknown`]) is where its owner is. The tables
    /// are in [`code::table_of`]'s order.
    pub fn span_tables(db: &Db, owner: DefWithBodyId) -> Vec<(ContentId, Vec<MirSpan>)> {
        let anchors = anchors(db, owner).0;
        let store = BodyStore::of(db, owner);
        let spans: [Vec<MirSpan>; TABLES] = [
            store.expr_ids().map(MirSpan::ExprId).collect(), // alloc: the tables returned, exact size
            store.expr_ids().map(MirSpan::ExprEnd).collect(), // alloc: the tables returned, exact size
            store.pat_ids().map(MirSpan::PatId).collect(), // alloc: the tables returned, exact size
            // alloc: the tables returned, exact size
            store.bindings().map(|(it, _)| MirSpan::BindingId(it)).collect(),
            vec![MirSpan::SelfParam], // alloc: the tables returned, one span
            vec![MirSpan::Unknown],   // alloc: the tables returned, one span
        ];
        anchors.into_iter().zip(spans).collect() // alloc: the tables returned, exact size
    }

    /// The anchor of the table [`Hashes::declaration`] names: one span, where `func` is
    /// declared.
    pub fn declaration_table(db: &Db, func: FunctionId) -> ContentId {
        let mut h = Hasher::new(SPAN_DOMAIN);
        h.id(&item_key(db, ItemId::Function(func)).0).update(&[DECLARATION]);
        h.finish()
    }
}

impl Hashes for ItemHashes {
    fn item(&self, db: &Db, item: ItemId) -> Result<ItemKey, String> {
        Ok(item_key(db, item))
    }

    fn code(&self, db: &Db, body: Body) -> Result<CodeHash, String> {
        blaze_query::db::attach(db, || {
            let (code, store) = match body {
                Body::Item(def) => (code::of_item(db, def), ExpressionStoreOwnerId::Body(def)),
                Body::Closure(id) => {
                    let id = InternedClosureId::from_id(id);
                    (code::of_closure(db, id), id.loc(db).owner.expression_store_owner(db))
                }
                Body::Coroutine(id) => {
                    let id = InternedCoroutineId::from_id(id);
                    (code::of_coroutine(db, id), id.loc(db).owner.expression_store_owner(db))
                }
                Body::CoroutineByMove(id) => {
                    let id = InternedCoroutineId::from_id(id);
                    (
                        code::of_coroutine_by_move(db, id),
                        id.loc(db).owner.expression_store_owner(db),
                    )
                }
            };
            let code = match code {
                Ok(code) => code,
                Err(why) => return Err(why.to_string()), // alloc: the error's message
            };
            let mut h = Hasher::new(OWNED_DOMAIN);
            h.id(code).id(&key::store_key(db, store).0);
            Ok(CodeHash(h.finish()))
        })
    }

    fn template_code(&self, db: &Db, body: Body) -> Result<CodeHash, String> {
        blaze_query::db::attach(db, || {
            let code = match body {
                Body::Item(def) => code::of_item(db, def),
                Body::Closure(id) => code::of_closure(db, InternedClosureId::from_id(id)),
                Body::Coroutine(id) => code::of_coroutine(db, InternedCoroutineId::from_id(id)),
                Body::CoroutineByMove(id) => {
                    code::of_coroutine_by_move(db, InternedCoroutineId::from_id(id))
                }
            };
            let code = code.as_ref().map_err(ToString::to_string)?; // alloc: the error's message
            let mut h = Hasher::new(TEMPLATE_CODE_DOMAIN);
            h.id(code);
            Ok(CodeHash(h.finish()))
        })
    }

    fn names_read(&self, db: &Db, body: Body, told: bool) -> Result<(ContentId, bool), String> {
        blaze_query::db::attach(db, || {
            let mut h = Hasher::new(NAMES_READ_DOMAIN);
            // What `file!()` bakes into its data as text: each file by the name the build gives
            // it, sorted, each once; a file the build named none is keyed as such, and the
            // code generator refuses it when it compiles the body.
            // alloc: none for a body without `file!()`, which is nearly every one
            let mut files: Vec<Option<String>> = match span_owner(db, body) {
                Some(owner) => (hir_ty_mir::mir::location::files_read(db, owner).into_iter())
                    .map(|file| (self.names)(file))
                    .collect(),
                None => Vec::new(),
            };
            files.sort_unstable();
            files.dedup();
            h.update(&(files.len() as u64).to_le_bytes());
            for file in &files {
                match file {
                    Some(name) => h
                        .update(&[1])
                        .update(&(name.len() as u64).to_le_bytes())
                        .update(name.as_bytes()),
                    None => h.update(&[0]),
                };
            }
            // What a debugger told of its locals reads of their types; a told local's type
            // holding an alias is not all of it (`told_names`).
            let whole = match told {
                false => {
                    h.update(&[0]);
                    true
                }
                true => match told_of(db, body)? {
                    Some(told) => {
                        h.update(&[1]).id(&told);
                        true
                    }
                    None => {
                        h.update(&[2]);
                        false
                    }
                },
            };
            Ok((h.finish(), whole))
        })
    }

    fn ty<'db>(&self, db: &'db Db, ty: Ty<'db>) -> Result<ContentId, String> {
        // alloc: the error's message
        blaze_query::db::attach(db, || identity::ty(db, ty).map_err(|why| why.to_string()))
    }

    fn konst<'db>(&self, db: &'db Db, konst: Const<'db>) -> Result<ContentId, String> {
        // alloc: the error's message
        blaze_query::db::attach(db, || identity::konst(db, konst).map_err(|why| why.to_string()))
    }

    fn span(&self, db: &Db, owner: DefWithBodyId, span: MirSpan) -> Result<SpanRef, String> {
        let Ok((table, index)) = code::table_of(span) else {
            // alloc: the error's message
            return Err(format!(
                "{span:?} of the body of {owner:?}, a span of code inlined into it, which is of \
                 the body it was inlined from: its place is the inlined span's"
            ));
        };
        Ok(SpanRef { anchor: anchors(db, owner).0[table], index })
    }

    fn anchors(&self, db: &Db, owner: DefWithBodyId) -> Result<[ContentId; TABLES], String> {
        Ok(anchors(db, owner).0)
    }

    fn declaration(&self, db: &Db, func: FunctionId) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: ItemHashes::declaration_table(db, func), index: 0 })
    }

    fn file_name(&self, _: &Db, file: base_db::FileId) -> Result<String, String> {
        (self.names)(file).ok_or_else(|| {
            // alloc: the error's message
            format!(
                "the file {file:?} has no name in the program: the build did not name it, so \
                 `file!()` and the positions of its code have none"
            )
        })
    }
}

/// What a debugger told of `body`'s locals reads of their types beyond its code and its
/// instance's arguments ([`code::told_names`]), `None` when a told local's type holds an
/// alias. Call it with the database attached.
pub(crate) fn told_of(db: &Db, body: Body) -> Result<Option<ContentId>, String> {
    let mir = match body {
        Body::Item(def) => db.mir_body(InferBodyId::DefWithBodyId(def)),
        Body::Closure(id) => db.mir_body_for_closure(InternedClosureId::from_id(id)),
        Body::Coroutine(id) => db.mir_body_for_coroutine(InternedCoroutineId::from_id(id)),
        Body::CoroutineByMove(id) => {
            db.mir_body_for_coroutine_by_move(InternedCoroutineId::from_id(id))
        }
    };
    // alloc: the error's message
    let mir = mir.map_err(|why| format!("the MIR of {body:?} failed to lower: {why:?}"))?;
    code::told_names(db, mir).map_err(|why| why.to_string()) // alloc: the error's message
}

/// The body whose span tables the positions of `body` are in ([`Hashes::span`]'s owner): an
/// item's own; a closure's or coroutine's, the body it is written in. `None` for one written in
/// an anonymous constant, which has no tables of its own.
pub(crate) fn span_owner(db: &Db, body: Body) -> Option<DefWithBodyId> {
    let owner = match body {
        Body::Item(def) => return Some(def),
        Body::Closure(id) => InternedClosureId::from_id(id).loc(db).owner,
        Body::Coroutine(id) | Body::CoroutineByMove(id) => {
            InternedCoroutineId::from_id(id).loc(db).owner
        }
    };
    match owner {
        InferBodyId::DefWithBodyId(def) => Some(def),
        InferBodyId::AnonConstId(_) => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Anchors([ContentId; TABLES]);

/// The anchors of a body's span tables: of the key of its store, which an edit of what the
/// body is keeps.
#[blaze_query::tracked(returns(copy))]
fn anchors(db: &Db, owner: DefWithBodyId) -> Anchors {
    let store = key::store_key(db, ExpressionStoreOwnerId::Body(owner));
    Anchors(std::array::from_fn(|table| {
        let mut h = Hasher::new(SPAN_DOMAIN);
        h.id(&store.0).update(&[table as u8]);
        h.finish()
    }))
}
