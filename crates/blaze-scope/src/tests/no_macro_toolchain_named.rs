//! A member proc macro the toolchain shelf cannot build is refused BY NAME where its names are
//! asked for (rule 3, rule 19), never resolved with no dependencies.
//!
//! The Reference, "Procedural Macros": "Procedural macros must be defined in the root of a
//! crate with the crate type of `proc-macro`", and such a crate is compiled for the machine
//! that runs it: blaze runs it on `wasm32-wasip1` (AGENTS.md rule 10), so its dependencies
//! are that target's crates. The Reference, "Extern prelude": the crates "provided to the
//! compiler" are what a path's first segment can name; with none given, `syn::Expr` names
//! nothing, which is not what the program says but what a missing toolchain did to it.
//!
//! The fixture is census 13 brief 5's ([`super::macro_side_names`]): `pm`, a member's proc
//! macro, depends on `dep`.

use blaze_manifest::lock::{Lock, LockId};
use blaze_toolchain::ToolchainView;

use super::macro_side_names::{HOST, LOCK, Texts, a, graph, index_of, seen, toolchain};
use super::{Member, UnitsError, unit_crates};
use crate::names::Why;
use crate::resolve::resolve_crates;
use crate::task::Serial;

/// Expected line: `test units::no_macro_toolchain_named::a_proc_macro_without_the_wasm_side_is_refused_by_name ... ok`.
///
/// Shelf without the `wasm32-wasip1` side: [`super::UnitCrates::checked_inputs`] gives no
/// inputs and the refusal of `pm`, whose words name `pm` and say "the toolchain shelf has no
/// wasm32-wasip1 side". CONTROL (the old path, the refusal unread): the same list's
/// `inputs()` resolve, and `dep::Expr` in `pm` is NotFound at segment 0, tempo's quiet class.
/// Shelf with the side: nothing is refused and `dep::Expr` in `pm` is the macro side's `dep`.
#[test]
fn a_proc_macro_without_the_wasm_side_is_refused_by_name() {
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    let members = [Member::new(LockId(0), "m0").expect("a plain dir"), Member::new(LockId(2), "m2").expect("a plain dir")];
    let g = graph();

    // The shelf without the macro side.
    let bytes = toolchain(false);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let uncut = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the list is made");
    let pm = index_of(&uncut, "pm", 0);
    let refused = match uncut.checked_inputs() {
        Ok(_) => panic!("a refused proc macro's names were handed on to be resolved"),
        Err(refused) => refused,
    };
    assert_eq!(refused.krate, pm, "the refusal is pm's");
    assert_eq!(refused.why, UnitsError::NoMacroToolchain { krate: "pm", target: "wasm32-wasip1" });
    let words = std::format!("{}", refused.why);
    assert!(words.contains("`pm`"), "the words name the proc macro: {words}");
    assert!(words.contains("the toolchain shelf has no wasm32-wasip1 side"), "the words say what fixes it: {words}");

    // CONTROL: the refusal unread, the old path resolves pm with no dependencies.
    {
        let texts = Texts::of(&uncut);
        let inputs = uncut.inputs();
        let names = resolve_crates(&inputs, uncut.sysroot, &texts, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        assert_eq!(seen(&names, pm, "Expr"), Err(Why::NotFound), "unread, the refusal is NotFound at segment 0");
    }

    // The shelf with the macro side: no refusal, and the path names the macro side's dep.
    let bytes = toolchain(true);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let cut = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the crates");
    let pm = index_of(&cut, "pm", 0);
    let side_dep = index_of(&cut, "dep", pm + 1);
    let inputs = cut.checked_inputs().unwrap_or_else(|r| panic!("with the side, nothing is refused: {}", r.why));
    let texts = Texts::of(&cut);
    let names = resolve_crates(&inputs, cut.sysroot, &texts, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    assert_eq!(seen(&names, pm, "Expr"), Ok(side_dep), "`dep::Expr` in the proc macro: the macro side's dep");
}
