//! The builtin derives, by hir-expand's token writers (`BuiltinDeriveExpander::expander`),
//! which parse the item from its tokens and write the impl. They take a `&base_db::Db` they do
//! not read (`parse_adt` names it `_db`).
//!
//! What hir-expand's own wrapper (`BuiltinDeriveExpander::expand`) adds besides is private to
//! it and reads the old database's call: `#[automatically_derived]` is added here instead;
//! the copy of the item's lint attributes onto the impl, the `*self` body of `Clone` for a
//! type that also derives `Copy`, and `derive_const` are not done yet.

use std::alloc::Allocator;

use crate::{
    bridge::{self, MarkImpls, Role},
    cx::ExpandCx,
    def::BuiltinDerive,
    error::{ExpandError, Stream},
    expand::Out,
    expansion::Diagnostic,
    tokens::Transparency,
};

/// Expands the derive `which` of the item in `out`.
pub(crate) fn expand<A: Allocator + Clone>(
    which: BuiltinDerive,
    out: &mut Out<'_, A>,
    cx: &impl ExpandCx,
) -> Result<(), ExpandError> {
    let edition = out.def.edition();
    let item = bridge::to_tt(out.call.input(), Role::Input, Stream::Input, edition)?;
    let writer = which.expander().expander();
    // rustc's builtin derives write at the definition's site, opaque.
    let site = bridge::site_span(edition);
    let db = cx.host_db();
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let result = writer(db, frame, site, &item);
        if let Some(error) = &result.err {
            let message = error.render_to_string(db).message;
            let message = out.note(&message)?;
            out.diagnose(Diagnostic::Derive { message });
            return Ok(());
        }
        bridge::from_tt(out, result.value.iter(), Transparency::Opaque, MarkImpls::Yes, 0)
    })
}
