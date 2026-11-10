//! What blaze appends to a proc-macro crate's root when it builds the crate for
//! `wasm32-wasip1` ([`wasm_glue`]): the module [`MODULE`], which lists the crate's macros as the
//! module numbers them (`listed`, sorted by the name each is invoked by, as
//! [`load_wasm`](crate::load_wasm) numbers them) and holds the bridge's glue
//! (`guest/entry.rs`), the module's exports the host calls ([`super::abi`]).
//!
//! Each listed macro is the pinned bridge's own `Client`, made by `Client::expand1` (a derive,
//! a function-like macro) or `expand2` (an attribute) of the macro's function, as rustc's
//! proc-macro harness makes it: the glue runs it through the client's `run` pointer.
//!
//! [`MODULE`] declares its own `extern crate std as __blaze_std`, and the list and the glue name
//! std only through it: a `#![no_std]` macro crate has no `std` in its extern prelude, and an
//! `extern crate std` at the crate's root would clash with one the crate declares itself.

use std::fmt::Write as _;

use base_db::{Crate, Db};
use hir_def::signatures::FunctionSignature;
use hir_expand::proc_macro::ProcMacroKind;

use crate::run;

/// The module's name in the crate's root.
pub const MODULE: &str = "__blaze_proc_macros";

/// The bridge's glue, put in [`MODULE`] after the list.
const ENTRY: &str = include_str!("../../guest/entry.rs");

/// The first line of the text, after the root file's: what a reader of the root as the
/// compiler sees it is told.
const MARK: &str = "\n// blaze's list of this crate's proc macros and its wasm module's glue, which are not in the file on disk.\n";

/// The text blaze appends to the root of `krate`, a proc-macro crate built for
/// `wasm32-wasip1`: [`MODULE`] with the list of its macros (none, for a crate that has none)
/// and the glue. An error names the crate when it is no proc-macro crate.
pub fn wasm_glue(db: &Db, krate: Crate) -> Result<String, String> {
    if !hir_def::nameres::crate_def_map(db, krate).is_proc_macro() {
        return Err(format!(
            "`{}` is not a proc-macro crate, so it has no wasm module's glue",
            krate.named(db)
        )); // alloc: error path
    }
    // A proc-macro crate with no macros is one rustc builds (`superset-of-rust`): its list is
    // empty, and a use of a macro it lacks fails at the use, naming it.
    let mut functions = run::functions(db, krate);
    // The order `load_on` numbers them in: by the name each is invoked by, stably.
    functions.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let edition = krate.data(db).edition;
    // alloc: once a proc-macro crate built for wasm, the text appended to its root
    let mut text = String::with_capacity(MARK.len() + ENTRY.len() + 256 + 128 * functions.len());
    text.push_str(MARK);
    text.push_str(
        "#[doc(hidden)]\n#[allow(warnings, clippy::all)]\npub mod __blaze_proc_macros {\n    \
         extern crate proc_macro as __blaze_proc_macro;\n    \
         extern crate std as __blaze_std;\n\n    \
         /// The bridge's client of the macro at `index`, the macros numbered by the name each is\n    \
         /// invoked by: made when asked, not in a static, as `Client::expand1` is no stable const fn.\n    \
         pub fn listed(index: u32) -> __blaze_std::option::Option<__blaze_proc_macro::bridge::client::Client> {\n        \
         __blaze_std::option::Option::Some(match index {\n",
    );
    for (at, (invoked, kind, function)) in functions.iter().enumerate() {
        let expand = match kind {
            ProcMacroKind::CustomDerive | ProcMacroKind::Bang => "expand1",
            ProcMacroKind::Attr => "expand2",
        };
        let own = FunctionSignature::of(db, *function).name.display(db, edition);
        _ = writeln!(
            text,
            "            // `{invoked}`\n            {at} => __blaze_proc_macro::bridge::client::Client::{expand}(super::{own}),",
            invoked = invoked.as_str(),
        );
    }
    text.push_str(
        "            _ => return __blaze_std::option::Option::None,\n        })\n    }\n\n",
    );
    text.push_str(ENTRY);
    text.push_str("}\n");
    Ok(text)
}
