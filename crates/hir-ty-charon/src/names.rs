//! Names: Charon's `Name` paths for items, and the `krate::module::item` strings errors use.
//!
//! charon-driver names an item by its rustc def path: the crate, each module, then the item; a
//! method has an `Impl` element for its impl block (the impl's self type) before its name. Every
//! disambiguator is zero: version 1 has no two items of one name in one namespace that rustc
//! would number apart.

use base_db::Db;
use charon_lib::ast::{
    Binder, BinderKind, BuiltinPathElem, Disambiguator, GenericParams, ImplElem, Name, PathElem,
    Ty as CTy,
};
use hir_def::{
    AdtId, HasModule as _, ModuleId,
    signatures::{EnumSignature, StructSignature, UnionSignature},
};

/// A crate's name, as its display name gives it.
pub(crate) fn crate_name(db: &Db, krate: base_db::Crate) -> String {
    krate
        .extra_data(db)
        .display_name
        .as_ref()
        .map(|it| it.crate_name().as_str().to_owned()) // alloc: a name, once per crate
        .unwrap_or_else(|| "crate".to_owned()) // alloc: a name, once per crate
}

/// The module path of `module` below its crate root, outermost first.
pub(crate) fn module_segments(db: &Db, module: ModuleId) -> Vec<String> {
    let mut segments = Vec::new(); // alloc: an item's module path, once per item
    let mut at = Some(module.to_db(db));
    while let Some(module) = at {
        if let Some(name) = module.name(db) {
            segments.push(name.as_str().to_owned()); // alloc: a name
        }
        at = module.containing_module(db);
    }
    segments.reverse();
    segments
}

/// An item's two names: Charon's path, and the string errors print.
#[derive(Clone, Debug)]
pub(crate) struct ItemPath {
    pub(crate) name: Name,
    pub(crate) display: String,
}

/// The impl block of a method: its self type as Charon has it, and as errors print it.
pub(crate) struct ImplOf {
    pub(crate) self_ty: CTy,
    pub(crate) display: String,
}

/// `krate::modules::[SelfTy::]item`.
pub(crate) fn item_path(
    krate: &str,
    modules: &[String],
    impl_of: Option<&ImplOf>,
    item: &str,
) -> ItemPath {
    let mut elems = Vec::with_capacity(modules.len() + 3); // alloc: a Charon name, once per item
    let mut display = String::from(krate); // alloc: a path, once per item
    elems.push(ident(krate));
    for module in modules {
        elems.push(ident(module));
        display.push_str("::");
        display.push_str(module);
    }
    if let Some(impl_of) = impl_of {
        elems.push(PathElem::Impl(ImplElem::Ty(Box::new(Binder {
            // alloc: a Charon name element, once per method
            params: GenericParams::empty(),
            skip_binder: impl_of.self_ty.clone(), // clone: a hash-consed handle
            kind: BinderKind::InherentImplBlock,
        }))));
        display.push_str("::");
        display.push_str(&impl_of.display);
    }
    elems.push(ident(item));
    display.push_str("::");
    display.push_str(item);
    ItemPath { name: Name { name: elems }, display }
}

/// A path of plain identifiers, as `core::intrinsics::add_with_overflow`.
pub(crate) fn plain_path(segments: &[&str]) -> ItemPath {
    ItemPath {
        name: Name { name: segments.iter().map(|it| ident(it)).collect() }, // alloc: a Charon name
        display: segments.join("::"),                                       // alloc: a path
    }
}

/// The name charon-driver gives the tuple type: one builtin element, `(...)` of arity 0.
pub(crate) fn tuple_name() -> Name {
    // alloc: a Charon name, once per crate
    Name { name: vec![PathElem::Builtin(BuiltinPathElem::Tuple(0), Disambiguator::ZERO)] }
}

fn ident(name: &str) -> PathElem {
    PathElem::Ident(name.to_owned(), Disambiguator::ZERO) // alloc: a Charon name element
}

/// An ADT's own name.
pub(crate) fn adt_name(db: &Db, adt: AdtId) -> String {
    match adt {
        AdtId::StructId(id) => StructSignature::of(db, id).name.as_str().to_owned(), // alloc: a name
        AdtId::EnumId(id) => EnumSignature::of(db, id).name.as_str().to_owned(), // alloc: a name
        AdtId::UnionId(id) => UnionSignature::of(db, id).name.as_str().to_owned(), // alloc: a name
    }
}

/// An ADT's path in its crate.
pub(crate) fn adt_path(db: &Db, adt: AdtId) -> ItemPath {
    let krate = crate_name(db, adt.krate(db));
    item_path(&krate, &module_segments(db, adt.module(db)), None, &adt_name(db, adt))
}

/// A Charon name as errors print it: its identifiers joined by `::`.
pub(crate) fn display(name: &Name) -> String {
    let mut out = String::new(); // alloc: an error report
    for (index, elem) in name.name.iter().enumerate() {
        if index > 0 {
            out.push_str("::");
        }
        match elem {
            PathElem::Ident(ident, _) => out.push_str(ident),
            PathElem::Impl(_) => out.push_str("{impl}"),
            PathElem::Builtin(BuiltinPathElem::Tuple(arity), _) => {
                out.push_str(&format!("(tuple/{arity})")); // alloc: an error report
            }
            PathElem::Builtin(..) | PathElem::Instantiated(..) | PathElem::Target(..) => {
                out.push_str("{builtin}");
            }
        }
    }
    out
}
