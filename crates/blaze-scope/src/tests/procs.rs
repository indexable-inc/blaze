//! Procedural macros, declared from their crate's source and run through the sources' runner
//! (here a stub standing for blaze-proc-macro's hook).
//!
//! The Reference, "Procedural Macros": a proc-macro crate exports its functions marked
//! `#[proc_macro]` (a function-like macro named by the function), `#[proc_macro_attribute]` (an
//! attribute macro named by the function) and `#[proc_macro_derive(Name, attributes(a, b))]` (the
//! derive `Name`, whose helper attributes `a` and `b` are inert on the item it is applied to).
//! They are in the macro namespace, so `pub use serde_derive::Serialize;` re-exports one like any
//! item, and a derive and a trait of one name live side by side ("Names", namespaces). A macro
//! used in a shape it does not accept is an error.

use std::format;
use std::vec::Vec;

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{DefKind, Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use crate::task::{ProcExport, Serial};

/// Crates resolved with no sysroot, one thread, from `sources`.
fn resolved<S: SourceTexts<'static>>(crates: &[CrateInput<'static>], sources: &'static S) -> Names<'static> {
    resolve_crates(crates, Sysroot::default(), sources, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

/// The proc-macro crate: one macro of each kind.
const MACROS: &str = "#![no_core]
#[proc_macro_derive(Show, attributes(show))]
pub fn show_derive(item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn replace(args: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro]
pub fn make(input: TokenStream) -> TokenStream { input }
#[proc_macro]
#[proc_macro_error]
pub fn sol(input: TokenStream) -> TokenStream { input }
";

/// A facade: re-exports the derive and declares a trait of the same name.
const DEP: &str = "#![no_core]
pub use macros::Show;
pub trait Show {}
";

const USER: &str = "#![no_core]
use dep::Show;
#[derive(Show)]
#[show(skip)]
pub struct Point;
#[macros::replace]
pub fn gone() {}
macros::make!();
#[derive(macros::make)]
pub struct Wrong;
macros::sol!();
";

/// The stub runner's key for the macro crate.
const RUNNER: CrateItemKey = CrateItemKey(ContentId([7; 32]));

/// Files as [`super::entry::Texts`], and a runner that knows crate 2 and writes a fixed text per
/// macro: the derive an impl, the attribute a fn in its item's place, the call a struct.
struct WithRunner(&'static [&'static [(&'static str, &'static str)]]);

impl SourceTexts<'static> for WithRunner {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 2).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let text = match (call.kind, def.proc_name()) {
            (ProcKind::Derive, Some("Show")) => "impl Show for Point {}",
            (ProcKind::Attr, Some("replace")) => "pub fn replaced() {}",
            (ProcKind::FnLike, Some("make")) => "pub struct Made;",
            (ProcKind::FnLike, Some("sol")) => "pub struct Sol;",
            _ => return Err(refused),
        };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: WithRunner = WithRunner(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)], &[("/macros/lib.rs", MACROS)]]);

fn crates() -> [CrateInput<'static>; 3] {
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    [
        input("user", "/user/lib.rs", &[("dep", 1), ("macros", 2)]),
        input("dep", "/dep/lib.rs", &[("macros", 2)]),
        CrateInput { implicit, ..macros },
    ]
}

#[test]
fn proc_macros_are_declared_with_their_shapes_and_run_through_the_runner() {
    let crates = crates();
    let names = resolved(&crates, &FILES);
    let g = Alloc::global();
    let at = |krate: usize, ns: Namespace, segments: &'static [&'static str]| {
        names.item_at(names.root(krate), ns, Written { kind: PathKind::Plain, segments }, g)
    };
    // Declared in the macro crate's root, each with the one shape its attribute gives.
    let shape = |name: &'static [&'static str]| at(2, Namespace::Macros, name).map(|it| it.def.kind).ok();
    assert_eq!(shape(&["Show"]), Some(DefKind::Macro(crate::MacroShapes::DERIVE)));
    assert_eq!(shape(&["replace"]), Some(DefKind::Macro(crate::MacroShapes::ATTR)));
    assert_eq!(shape(&["make"]), Some(DefKind::Macro(crate::MacroShapes::FN_LIKE)));
    // A derive is named by its attribute, not by its function, which stays a value.
    assert!(at(2, Namespace::Macros, &["show_derive"]).is_err());
    assert!(at(2, Namespace::Values, &["show_derive"]).is_ok());
    // Re-exported, the derive and the trait of one name are both there: two namespaces.
    let derive = at(1, Namespace::Macros, &["Show"]).unwrap_or_else(|e| panic!("dep's re-exported derive: {e}"));
    let trait_ = at(1, Namespace::Types, &["Show"]).unwrap_or_else(|e| panic!("dep's trait: {e}"));
    assert_ne!(derive.def.key, trait_.def.key);
    assert_eq!(derive.krate, 2, "the re-export names the macro crate's derive");

    let user = names.krate(0);
    let root = names.root(0);
    let report = || format!("notes {:?}; errors {:?}", names.notes(0), user.done.get(&root).map(|d| &d.scope.errors));
    // The derive, found through `use dep::Show`, writes its impl; `#[show]` is its helper.
    let headers: Vec<&str> = user.tables.impls.iter().map(|i| i.header).collect();
    assert!(headers.iter().any(|h| h.contains("Show") && h.contains("Point")), "the derive's impl: {headers:?}; {}", report());
    // The attribute macro replaces its item.
    assert!(at(0, Namespace::Values, &["replaced"]).is_ok(), "the attribute's output: {}", report());
    assert!(at(0, Namespace::Values, &["gone"]).is_err(), "the attribute's input is not declared");
    // The function-like call at item position declares what it writes.
    assert!(at(0, Namespace::Types, &["Made"]).is_ok(), "the call's output: {}", report());
    // A function-like macro used as a derive is a typed finding naming it.
    let errors = &user.done.get(&root).expect("the root is done").scope.errors;
    let wrong = errors.iter().any(|e| {
        matches!(e, crate::ScopeError::Macro { path, why: crate::Unresolved::WrongMacroKind, .. } if path.segments.join("::") == "macros::make")
    });
    assert!(wrong, "a wrong-shape finding: {errors:?}");
}

/// alloy-sol-macro 1.5's shape: `#[proc_macro] #[proc_macro_error] pub fn sol`. The Reference,
/// "Procedural Macros": a proc-macro crate exports only its procedural macros, named by their
/// functions; another attribute on the function can change its body, which nothing outside the
/// crate reads, not what the crate exports. So `sol!` is declared from the text and runs.
#[test]
fn a_proc_macro_under_another_attribute_macro_is_declared_and_runs() {
    let crates = crates();
    let names = resolved(&crates, &FILES);
    let g = Alloc::global();
    let at = |krate: usize, ns: Namespace, segments: &'static [&'static str]| {
        names.item_at(names.root(krate), ns, Written { kind: PathKind::Plain, segments }, g)
    };
    let sol = at(2, Namespace::Macros, &["sol"]).unwrap_or_else(|e| panic!("`sol`, under `#[proc_macro_error]`: {e}; notes {:?}", names.notes(2)));
    assert_eq!(sol.def.kind, DefKind::Macro(crate::MacroShapes::FN_LIKE));
    assert!(at(0, Namespace::Types, &["Sol"]).is_ok(), "`macros::sol!()` ran: notes {:?}", names.notes(0));
}

/// asn1-rs-derive 0.6's shape: its derives are declared through another macro,
/// `synstructure::decl_derive!([DerSequence, attributes(debug_derive)] => derive_der_sequence);`,
/// whose crate is the macro crate's host dependency: no `#[proc_macro_derive]` is written in its
/// text. The Reference, "Procedural Macros": "Procedural macros must be defined in the root of a
/// crate with the crate type of proc-macro"; what a proc-macro crate exports is its procedural
/// macros, so the truth is the built module's own list (what it declares to the host), not a
/// reading of its text: the derive is declared from the list, with its helper.
#[test]
fn a_macro_crate_s_exports_come_from_its_module_s_list() {
    const DECLARED: &str = "#![no_core]
synstructure::decl_derive!([DerSequence, attributes(debug_derive)] => derive_der_sequence);
fn derive_der_sequence(s: synstructure::Structure) -> TokenStream { s }
#[proc_macro]
pub fn only_in_text(input: TokenStream) -> TokenStream { input }
";
    const USER: &str = "#![no_core]
use macros::DerSequence;
#[derive(DerSequence)]
#[debug_derive]
pub struct Point;
";
    static LIST: [ProcExport<'static>; 1] = [ProcExport { kind: ProcKind::Derive, name: "DerSequence", helpers: "debug_derive" }];
    struct Listed;
    impl SourceTexts<'static> for Listed {
        fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
            [("/user/lib.rs", USER), ("/macros/lib.rs", DECLARED)].get(krate).filter(|(p, _)| *p == path).map(|(_, t)| *t)
        }
        fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
            (krate == 1).then_some(RUNNER)
        }
        fn proc_exports(&self, krate: usize) -> Option<&'static [ProcExport<'static>]> {
            (krate == 1).then_some(&LIST[..])
        }
    }
    static LISTED: Listed = Listed;
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("macros", 1)]), CrateInput { implicit, ..input("macros", "/macros/lib.rs", &[]) }];
    let names = resolved(&crates, &LISTED);
    let g = Alloc::global();
    let at = |krate: usize, ns: Namespace, segments: &'static [&'static str]| {
        names.item_at(names.root(krate), ns, Written { kind: PathKind::Plain, segments }, g)
    };
    let derive = at(1, Namespace::Macros, &["DerSequence"]).unwrap_or_else(|e| panic!("the module's derive: {e}; notes {:?}", names.notes(1)));
    assert_eq!(derive.def.kind, DefKind::Macro(crate::MacroShapes::DERIVE));
    // The list is the whole truth: a text attribute the module does not list declares nothing.
    assert!(at(1, Namespace::Macros, &["only_in_text"]).is_err(), "only the module's list is declared");
    let imported = at(0, Namespace::Macros, &["DerSequence"]).unwrap_or_else(|e| panic!("`use macros::DerSequence`: {e}; notes {:?}", names.notes(0)));
    assert_eq!(imported.krate, 1);
}

#[test]
fn proc_macro_attributes_read_as_the_reference_says() {
    use crate::task::ProcAttr;
    let derive = ProcAttr::read("proc_macro_derive(Serialize, attributes(serde, r#as))").expect("a derive");
    assert_eq!((derive.kind, derive.derive), (ProcKind::Derive, Some("Serialize")));
    assert!(ProcAttr::helper_names(derive.helpers).eq(["serde", "as"]));
    let bare = ProcAttr::read("proc_macro_derive(Plain)").expect("a derive");
    assert_eq!(ProcAttr::helper_names(bare.helpers).count(), 0);
    assert_eq!(ProcAttr::read("proc_macro_attribute").map(|it| it.kind), Some(ProcKind::Attr));
    assert_eq!(ProcAttr::read("proc_macro").map(|it| it.kind), Some(ProcKind::FnLike));
    assert_eq!(ProcAttr::read("proc_macro_derive()"), None, "a derive with no name exports nothing");
}

/// Without a runner for its crate, a proc macro still resolves and each call reports the missing runner.
#[test]
fn a_proc_macro_without_a_runner_resolves_and_is_not_run() {
    use crate::resolve::{ExpandNote, NamesNote, ProcUnusable};

    struct NoRunner;
    impl SourceTexts<'static> for NoRunner {
        fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
            FILES.read(krate, path)
        }
    }
    static NONE: NoRunner = NoRunner;
    let crates = crates();
    let names = resolved(&crates, &NONE);
    let user = names.krate(0);
    assert!(user.tables.impls.is_empty(), "nothing derived without a runner");
    let notes = names.notes(0);
    let mut without_runner: Vec<_> = notes.iter().filter_map(|note| match note {
        NamesNote::Expand { note: ExpandNote::ProcModule { macro_name, why: ProcUnusable::NoRunner }, .. } => Some(*macro_name),
        _ => None,
    }).collect();
    without_runner.sort_unstable();
    assert_eq!(without_runner, ["Show", "make", "replace", "sol"], "each call is noted: {notes:?}");
    let errors = &user.done.get(&names.root(0)).expect("the root is done").scope.errors;
    assert!(
        !errors.iter().any(|e| matches!(e, crate::ScopeError::Macro { why: crate::Unresolved::NotFound, .. })),
        "every proc-macro path resolves: {errors:?}"
    );
}
