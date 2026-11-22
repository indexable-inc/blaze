//! A signature's type with its type macros expanded: one home for lowering a type's own text
//! with every macro call in it expanded, for [`super::pattern_ty`]'s `macro_ty` (one type macro
//! in a type) and for `heads.rs` `fn_head` (a fn's whole return type when a macro is written
//! anywhere in it).
//!
//! A signature is lowered unexpanded, so a type macro in it is a [`lf::TypeRef::Macro`] node
//! whose type is known only after its expansion. The Reference ("Impl trait", "Abstract return
//! types") says each `impl Trait` in a function's return type is an opaque type of that
//! function, and a type macro's expansion is read as the type it writes ("Macros By Example":
//! "When a macro is invoked ... the macro expander ... transcribes" the matched rule's body "in
//! place of" the call), so `fn f() -> fut!(u8)` with `macro_rules! fut { ($t:ty) => { impl
//! Future<Output = $t> } }` is `fn f() -> impl Future<Output = u8>`, opaque 0 of `f`. The opaques
//! of a return type are therefore numbered over its EXPANSION ([`Expanded::impls`]).
//!
//! The expansion is a separate lowering with its own view: its type ids and ranges are its own
//! and are never compared with the item's ([`blaze_lower::View::site`] refuses a range of another
//! view; a type id carries no view, so callers keep each view's ids in their own list).

use std::alloc::Global;
use std::format;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_lower::by_name::ByName;
use blaze_lower::{Lowered, View, form as lf};
use blaze_own::Alloc;
use blaze_surface::Range;

use super::body::NoHost;
use super::heads::CrateCfg;
use super::scoped::ScopedBy;
use super::source::NamesSource;
use super::types::{Why, return_impls};

/// A type's own text lowered with every macro call in it expanded, and the type it is.
pub(super) struct Expanded<'a, 't> {
    lowered: Lowered<'a, 't>,
    /// The expanded type, in [`Expanded::view`].
    pub root: lf::TypeId,
}

impl<'a, 't> Expanded<'a, 't> {
    /// The expansion's own view: [`Expanded::root`] and every type id from it are ids of this
    /// view alone.
    pub(super) fn view(&self) -> View<'_> {
        self.lowered.view()
    }

    /// The expansion's `impl Trait` types in [`return_impls`]' order (preorder, left to right,
    /// an `impl` before those in its bounds' arguments), appended to `out`. An expansion that is
    /// no type, or that leaves a macro call, is refused naming `text` (the written type): a
    /// signature's type is expanded once, never again (bounded, as `macro_ty` was).
    pub(super) fn impls(&self, text: &str, r: Range, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
        let v = self.view();
        if let lf::TypeRef::Error = v.ty(self.root)? {
            return Err(Why::TypeMacro(format!("`{text}` (its expansion is no type)").into_boxed_str(), r));
        }
        if return_impls(&v, self.root, out)? {
            return Err(Why::TypeMacro(format!("`{text}` (a macro call its expansion leaves)").into_boxed_str(), r));
        }
        Ok(())
    }
}

/// The type written as `text` (a signature's type, read from its view) in module `module`, with
/// every macro call in it expanded by the names of that module and its crate's cfg and edition;
/// `r` is the written type's place in the item, for a refusal; the expansion is made in `alloc`.
pub(super) fn expand_type_text<'n, 'a, 't, S: NamesSource<'n>>(
    names: S,
    module: LibKey,
    text: &'t str,
    marks: Option<blaze_scope::crate_roots::TextMarks<'_>>,
    r: Range,
    alloc: Alloc<'a>,
) -> Result<Expanded<'a, 't>, Why> {
    let krate = names.crate_of(module).ok_or_else(|| Why::TypeMacro(format!("`{text}`: the crate of module {module:x} is not read").into_boxed_str(), r))?;
    let atoms = names.cfg_atoms(krate).ok_or_else(|| Why::TypeMacro(format!("`{text}`: the cfg of the crate of module {module:x} is not read").into_boxed_str(), r))?;
    let edition = names.edition_of(krate).ok_or_else(|| Why::TypeMacro(format!("`{text}`: the edition of the crate of module {module:x} is not read").into_boxed_str(), r))?;
    // A path an expansion wrote is read by its token's mark (`$crate` in the defining crate):
    // front2/type_marks.rs.
    let scoped = super::type_marks::Marked { scoped: ScopedBy { names, module, prepared: None, blocks: None }, marks };
    let mut expand = ByName::with_names(&scoped, NoHost, alloc);
    let lowered = blaze_lower::lower_type_text_expanding(text, edition, &mut CrateCfg(atoms), &mut expand, alloc)
        .map_err(|e| Why::TypeMacro(format!("`{text}`: {e}").into_boxed_str(), r))?;
    let root = lowered.view().type_root().ok_or_else(|| Why::TypeMacro(format!("`{text}` expanded to no type").into_boxed_str(), r))?;
    Ok(Expanded { lowered, root })
}
