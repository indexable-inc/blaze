//! Whose a query's key is: the edited body's own, its file's, or another item's.
//!
//! A key is a query database [`Id`]: an interned or input struct a query takes alone, or the
//! interned arguments of a query that takes several, whose fields list their ids
//! ([`blaze_query::db::Storage::ids_of`]).
//! Each id is decoded by its ingredient: a closure, coroutine, opaque type or anonymous
//! constant by the body it is written in, a block, block module or nested function by where
//! its syntax is, a macro call by the body whose request first interned it, and a file and
//! its text by the file.

use base_db::{Db, EditionedFileId, FileId, FileText};
use blaze_hash::HashMap;
use blaze_query::db::{AsId, Database as _, FromId, Id};
use hir_def::{BlockIdLt, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, Lookup, ModuleIdLt};
use hir_expand::{AstId, HirFileId};
use hir_ty::{
    ImplTraitId, InferBodyId,
    db::{
        AnonConstId, InternedClosureId, InternedCoroutineClosureId, InternedCoroutineId,
        InternedOpaqueTyId,
    },
};
use syntax::{AstNode, TextRange};

/// How deep the fields of interned arguments are followed: a query's arguments, the
/// arguments' own interned fields, and one level more (a type naming a closure).
const DEPTH: u32 = 3;

/// The body an edit is in, as the keys are told apart by it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Edited {
    pub(crate) def: DefWithBodyId,
    /// Its place in the harness's list of bodies: what [`Interned`] names it by.
    pub(crate) index: u32,
    /// The file its syntax is in, and the range of its item's syntax there, as the text is
    /// now (with the edit, or without it once undone).
    pub(crate) file: HirFileId,
    pub(crate) vfs: FileId,
    pub(crate) range: TextRange,
}

/// The macro calls each body's request interned first, by the body's place in the list: a
/// macro call is a body's when its expansion is in the body's syntax, and the first to intern
/// it is the lowering of the body it is written in. Item macro calls are interned before any
/// body is asked for: the module checks run first.
pub(crate) type Interned = HashMap<Id, u32>;

/// What a key is to the edit of a body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Whose {
    /// Keyed by the body, or by something interned from it.
    Own,
    /// Keyed by the edited file or its text: a query that executes again on any edit of the
    /// file's text, and gives what it gave when the text outside the body is the same.
    File,
    /// Anything else.
    Other,
}

/// What `key` (a query's key) is to `edited`.
pub(crate) fn whose(db: &Db, interned: &Interned, edited: &Edited, key: Id) -> Whose {
    // alloc: tests only, a few ids per key
    let mut ids = Vec::new();
    whose_at(db, interned, edited, key, DEPTH, &mut ids)
}

fn whose_at(db: &Db, interned: &Interned, edited: &Edited, id: Id, depth: u32, ids: &mut Vec<Id>) -> Whose {
    if id == edited.def.as_id() {
        return Whose::Own;
    }
    let storage = db.storage();
    let name = storage.owner(id).map_or("", |it| storage.ingredient_name(it));
    let own = |yes: bool| if yes { Whose::Own } else { Whose::Other };
    // A closure or coroutine is the body's when it is written in it, or in an anonymous
    // constant of its expressions.
    let owned = |owner: InferBodyId<'_>| {
        own(owner.expression_store_owner(db) == ExpressionStoreOwnerId::Body(edited.def))
    };
    match name {
        "InternedClosureId" => owned(InternedClosureId::from_id(id).loc(db).owner),
        "InternedCoroutineId" => owned(InternedCoroutineId::from_id(id).loc(db).owner),
        "InternedCoroutineClosureId" => owned(InternedCoroutineClosureId::from_id(id).loc(db).owner),
        "InternedOpaqueTyId" => own(match InternedOpaqueTyId::from_id(id).loc(db) {
            ImplTraitId::ReturnTypeImplTrait(func, _) => DefWithBodyId::from(func) == edited.def,
            ImplTraitId::TypeAliasImplTrait(..) => false,
        }),
        "AnonConstId" => own(AnonConstId::from_id(id).loc(db).owner == ExpressionStoreOwnerId::Body(edited.def)),
        "BlockIdLt" => own(within(db, edited, BlockIdLt::from_id(id).ast_id(db))),
        "ModuleIdLt" => own(ModuleIdLt::from_id(id).block(db).is_some_and(|block| within(db, edited, block.ast_id(db)))),
        "FunctionId" => own(within(db, edited, FunctionId::from_id(id).lookup(db).id)),
        "MacroCallId" => own(interned.get(&id) == Some(&edited.index)),
        "EditionedFileId" if EditionedFileId::from_id(id).file_id(db) == edited.vfs => Whose::File,
        "FileText" if *FileText::from_id(id).file_id(db) == edited.vfs => Whose::File,
        _ if depth == 0 => Whose::Other,
        _ => {
            let start = ids.len();
            storage.ids_of(id, ids);
            let mut found = Whose::Other;
            for at in start..ids.len() {
                match whose_at(db, interned, edited, ids[at], depth - 1, ids) {
                    Whose::Own => {
                        found = Whose::Own;
                        break;
                    }
                    Whose::File => found = Whose::File,
                    Whose::Other => {}
                }
            }
            ids.truncate(start);
            found
        }
    }
}

/// Whether the syntax `at` names is inside the edited body's item.
fn within<N: AstNode>(db: &Db, edited: &Edited, at: AstId<N>) -> bool {
    at.file_id == edited.file && edited.range.contains_range(at.to_range(db))
}
