//! The list of a proc-macro crate's macros, which rustc keeps in the crate's metadata and the
//! loader of ra-jit's proc-macro server reads from the library ra-jit links instead
//! (docs/no-rustc.md, step J2). ra-jit adds it to the root file of a proc-macro crate it links
//! as a shared library, as a module of its own as it does a test harness: a static exported
//! by name with each macro's kind, name, helper attributes and the client the server expands
//! it through.

use std::fmt::Write as _;

use ide_db::RootDatabase;

/// The module's name in the crate's root.
pub(crate) const MODULE: &str = "__ra_jit_proc_macros";

/// What the loader looks the list up by: a slice of the macros, as rustc's
/// `#[rustc_proc_macro_decls]` static is.
pub(crate) const SYMBOL: &str = "RA_JIT_PROC_MACROS";

/// What the module starts with, after the text of the root file.
pub(crate) const MARK: &str =
    "\n// ra-jit's list of the crate's proc macros, which is not in the file on disk.\n";

/// The module for the macros of `krate`, a proc-macro crate.
pub(crate) fn module(db: &RootDatabase, krate: hir::Crate) -> String {
    let edition = krate.edition(db);
    let mut macros = Vec::new();
    for def in krate.root_module(db).declarations(db) {
        let hir::ModuleDef::Function(function) = def else { continue };
        let Some(mac) = function.as_proc_macro(db) else { continue };
        let path = function.name(db).display(db, edition).to_string();
        // The kinds as ra-jit-expand numbers them.
        let (kind, helpers) = match mac.kind(db) {
            hir::MacroKind::Derive => {
                let id = hir_def::MacroId::from(mac);
                let helpers = hir_def::attrs::AttrFlags::derive_info(db, id)
                    .map(|it| it.helpers.iter().map(|it| it.as_str().to_owned()).collect())
                    .unwrap_or_default();
                (0, helpers)
            }
            hir::MacroKind::Attr => (1, Vec::new()),
            _ => (2, Vec::new()),
        };
        macros.push((kind, mac.name(db).as_str().to_owned(), helpers, path));
    }
    // rustc lists them as the crate declares them; the server finds them by name.
    macros.sort_by(|a, b| a.1.cmp(&b.1));
    text(&macros)
}

fn text(macros: &[(u32, String, Vec<String>, String)]) -> String {
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
    for (kind, name, helpers, path) in macros {
        let expand = if *kind == 1 { "expand2" } else { "expand1" };
        _ = writeln!(
            text,
            "        Macro {{ kind: {kind}, name: {name:?}, helpers: &{helpers:?}, \
             client: Client::{expand}(crate::{path}) }},"
        );
    }
    text.push_str("    ];\n}\n");
    text
}
