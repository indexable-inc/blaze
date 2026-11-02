//! The list of a proc-macro crate's macros, which rustc keeps in the crate's metadata and its
//! `#[rustc_proc_macro_decls]` static, and blaze in a static exported by name, `BLAZE_PROC_MACROS`,
//! that the loader of blaze's proc-macro server reads (`load-cargo`'s `listed`): each macro's
//! kind, name, helper attributes and the client the server expands it through. It is source
//! text, a module [`MODULE`] the build adds to the end of the crate's root file
//! ([`HirFrontend::proc_macro_list`]), written as v1 writes it, so either's library loads.

use std::fmt::Write as _;

use base_db::{Crate, Db};
use blaze_codegen::CodegenError;
use hir_def::{
    FunctionId, Lookup, MacroId, ModuleDefId, attrs::AttrFlags, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::{name::Name, proc_macro::ProcMacroKind};
use intern::Symbol;

use super::{super::HirFrontend, Listed, refused};
use crate::home::Homes;

/// The module's name in the crate's root.
pub const MODULE: &str = "__blaze_proc_macros";

/// What the module starts with, after the text of the root file: v1's mark.
pub const MARK: &str =
    "\n// blaze's list of the crate's proc macros, which is not in the file on disk.\n";

/// What the loader looks the list up by (`load-cargo`'s `listed`): a slice of the macros, as
/// rustc's `#[rustc_proc_macro_decls]` static is.
pub const SYMBOL: &str = "BLAZE_PROC_MACROS";

/// One macro of the list, as [`list_text`] writes it.
pub struct ListedMacro<'a> {
    /// As `proc_macro_api::kept` numbers them: a derive 0, an attribute 1, a function-like 2.
    pub kind: u32,
    /// What it is invoked by: a derive's trait, else its function's name.
    pub name: &'a str,
    pub helpers: &'a [String],
    /// Its function, from the crate's root.
    pub path: &'a str,
}

/// One macro of the list.
struct Macro {
    /// As `proc_macro_api::kept` numbers them: a derive 0, an attribute 1, a function-like 2.
    kind: u32,
    /// What it is invoked by: a derive's trait, else its function's name.
    name: String,
    helpers: Vec<String>,
    /// Its function, from the crate's root.
    path: String,
    function: FunctionId,
}

/// The macros of `krate`, sorted by name: the server finds them by name.
fn macros(db: &Db, krate: Crate) -> Result<Vec<Macro>, String> {
    let def_map = crate_def_map(db, krate);
    if !def_map.is_proc_macro() {
        // alloc: the error's message
        return Err("it is not a proc-macro crate".to_owned());
    }
    let edition = krate.data(db).edition;
    // alloc: once a proc-macro crate, its list
    let mut macros = Vec::new();
    for def in def_map.module(db, def_map.crate_root(db)).scope(db).declarations() {
        let ModuleDefId::FunctionId(function) = def else { continue };
        let Some(id) = def_map.fn_as_proc_macro(function) else { continue };
        // alloc: once a macro, the list's text
        let path = FunctionSignature::of(db, function).name.display(db, edition).to_string();
        let (kind, name, helpers) = match id.lookup(db).kind {
            ProcMacroKind::CustomDerive => {
                let Some(info) = AttrFlags::derive_info(db, MacroId::ProcMacroId(id)) else {
                    // alloc: the error's message
                    return Err(format!(
                        "the derive macro `{path}` has no trait name its attribute gives"
                    ));
                };
                // alloc: once a derive, the list's text
                let helpers = info.helpers.iter().map(|it| it.as_str().to_owned()).collect();
                // alloc: as above
                (0, info.trait_name.as_str().to_owned(), helpers)
            }
            ProcMacroKind::Attr => (1, path.clone(), Vec::new()), // clone: once a macro
            ProcMacroKind::Bang => (2, path.clone(), Vec::new()), // clone: once a macro
        };
        macros.push(Macro { kind, name, helpers, path, function });
    }
    macros.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(macros)
}

/// The roots of a proc-macro crate's library, past those of a program ([`super`]): each
/// macro's function, which the list's static, a kept symbol, also reaches. The root file must
/// end with the list.
pub(super) fn roots(
    db: &Db,
    homes: &mut Homes,
    krate: Crate,
    listed: &mut Listed,
) -> Result<(), CodegenError> {
    let macros = macros(db, krate).map_err(|what| refused(db, krate, what))?;
    let def_map = crate_def_map(db, krate);
    let root = def_map.module(db, def_map.crate_root(db));
    if !root.children(db).contains_key(&Name::new_symbol_root(Symbol::intern(MODULE))) {
        return Err(refused(
            db,
            krate,
            // alloc: the error's message
            format!(
                "its root file has no list of its proc macros (the module `{MODULE}` the build \
                 writes with `HirFrontend::proc_macro_list`)"
            ),
        ));
    }
    for it in macros {
        listed.function(db, homes, it.function);
    }
    Ok(())
}

impl HirFrontend<'_> {
    /// The module listing the proc macros of `krate`, a proc-macro crate, to add to the end of
    /// the text of its root file ([`Self::on_disk`] gives the text before).
    pub fn proc_macro_list(&self, krate: Crate) -> Result<String, CodegenError> {
        self.with(|db, _| {
            let macros = macros(db, krate).map_err(|what| refused(db, krate, what))?;
            Ok(list_text(macros.iter().map(|it| ListedMacro {
                kind: it.kind,
                name: &it.name,
                helpers: &it.helpers,
                path: &it.path,
            })))
        })
    }
}

/// The module [`MODULE`] listing `macros`, in the order given, which the server finds them in
/// by name: the text v1 and v2 both append.
pub fn list_text<'a>(macros: impl IntoIterator<Item = ListedMacro<'a>>) -> String {
    // alloc: once a proc-macro crate, the text cargo writes
    let mut text = String::from(MARK);
    _ = write!(
        text,
        "#[doc(hidden)]\n#[allow(warnings, clippy::all)]\npub mod {MODULE} {{\n    \
         extern crate proc_macro;\n\n    \
         use proc_macro::bridge::client::Client;\n\n    \
         #[repr(C)]\n    \
         pub struct Macro {{\n        \
         pub kind: u32,\n        \
         pub name: &'static str,\n        \
         pub helpers: &'static [&'static str],\n        \
         pub client: Client,\n    \
         }}\n\n    \
         #[unsafe(no_mangle)]\n    \
         pub static {SYMBOL}: &[Macro] = &[\n",
    );
    for ListedMacro { kind, name, helpers, path } in macros {
        let expand = if kind == 1 { "expand2" } else { "expand1" };
        _ = writeln!(
            text,
            "        Macro {{ kind: {kind}, name: {name:?}, helpers: &{helpers:?}, \
             client: Client::{expand}(crate::{path}) }},"
        );
    }
    text.push_str("    ];\n}\n");
    text
}
