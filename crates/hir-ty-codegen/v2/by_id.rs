//! The tests' [`Hashes`]: content IDs of the database's own ids.

use base_db::Db;
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemId, ItemKey};
use hir_def::{DefWithBodyId, FunctionId};
use hir_ty::next_solver::{Const, Ty};
use hir_ty_mir::mir::MirSpan;
use hir_ty_share::code::{self, TABLES};

use super::{Body, Hashes};

/// Content IDs of the database's own ids: stable within one database, which is all a test
/// compares.
pub(crate) struct ById;

fn by_id(domain: &'static str, what: impl std::fmt::Debug) -> ContentId {
    // alloc: the tests' hash source, which no real build uses
    ContentId::of(domain, format!("{what:?}").as_bytes())
}

impl Hashes for ById {
    fn item(&self, _: &Db, item: ItemId) -> Result<ItemKey, String> {
        Ok(ItemKey(by_id("test item", item)))
    }

    fn code(&self, _: &Db, body: Body) -> Result<CodeHash, String> {
        Ok(CodeHash(by_id("test code", body)))
    }

    fn ty<'db>(&self, db: &'db Db, ty: Ty<'db>) -> Result<ContentId, String> {
        Ok(hir_ty::attach_db(db, || by_id("test type", ty)))
    }

    fn konst<'db>(&self, db: &'db Db, konst: Const<'db>) -> Result<ContentId, String> {
        Ok(hir_ty::attach_db(db, || by_id("test const", konst)))
    }

    fn span(&self, _: &Db, owner: DefWithBodyId, span: MirSpan) -> Result<SpanRef, String> {
        let (table, index) = code::table_of(span)
            // alloc: an error
            .map_err(|at| format!("{span:?} of {owner:?}: inlined span {at}, which is of another body"))?;
        Ok(SpanRef { anchor: by_id("test span", (owner, table)), index })
    }

    fn anchors(&self, _: &Db, owner: DefWithBodyId) -> Result<[ContentId; TABLES], String> {
        Ok(std::array::from_fn(|table| by_id("test span", (owner, table))))
    }

    fn declaration(&self, _: &Db, func: FunctionId) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: by_id("test declaration", func), index: 0 })
    }

    fn file_name(&self, _: &Db, file: base_db::FileId) -> Result<String, String> {
        Ok(format!("{file:?}")) // alloc: the tests' hash source, which no real build uses
    }
}
