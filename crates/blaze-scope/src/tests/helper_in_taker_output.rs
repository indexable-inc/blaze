//! C01 (wave 2311): a derive's helper stays inert in the output of the attribute macro that took
//! the item after that derive, and an attribute path written after a derive runs after it.
//!
//! tempo's build census 7 (2026-11-16): about 6,700 of its 8,023 left-out functions sit in impls
//! whose self type is NotFound in its own module, all of two shapes. reth-codecs'
//! `#[derive(.., Compact)] #[reth_codecs(crate = "crate")] #[add_arbitrary_tests(crate, compact)]
//! pub(crate) struct StorageEntry` (`Compact` declares the helper `reth_codecs`;
//! `add_arbitrary_tests` returns `#ast #tests`, the item with `#[reth_codecs(..)]` still on it),
//! and derive_where 1.7's `#[derive_where(Debug)] struct PoolResults<T>`, whose output is
//! `#[derive(::derive_where::DeriveWhere)] #[derive_where(Debug)] struct .. ` with
//! `#[::derive_where::derive_where_visited]` (a no-op attribute macro) last. rustc takes the
//! attributes in written order (`take_first_attr`): the derive, then the path, whose output keeps
//! `#[derive_where(..)]`; there, and in `add_arbitrary_tests`' output, the helper resolves through
//! `Scope::DeriveHelpers` of the parent expansion. blaze sent the path before the derive and, in
//! both outputs, called the helper as an attribute macro: `reth_codecs` failed NotFound and
//! `derive_where` called itself up to the recursion limit, so the struct was never declared.

use std::string::String;
use std::vec::Vec;

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallRole, Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{CrateInput, CrateNames, SourceTexts, resolve_crates};
use crate::task::Serial;
use crate::{CallShape, ScopeError, Unresolved};

/// A derive declaring the helper `codec` (reth-codecs' `Compact`), an attribute macro that writes
/// the item back with a test module (`add_arbitrary_tests`), and a no-op attribute macro named by
/// a path (`derive_where_visited`).
const MACROS: &str = "#![no_core]
#[proc_macro_derive(Codec, attributes(codec))]
pub fn codec(item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn add_tests(args: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn visited(args: TokenStream, item: TokenStream) -> TokenStream { item }
";

const USER: &str = "#![no_core]
use macros::{Codec, add_tests};
#[derive(Codec)]
#[codec(crate = \"x\")]
#[add_tests(compact)]
pub struct Entry {
    pub f: u8,
}
#[derive(Codec)]
#[codec(skip)]
#[macros::visited]
pub struct Seen;
";

const RUNNER: CrateItemKey = CrateItemKey(ContentId([9; 32]));

/// The files, and a runner for crate 1 writing what each macro returns for these items: the derive
/// nothing, each attribute macro the item as its input holds it (its derive and its own attribute
/// cut, the helper kept), `add_tests` with a test module after it.
struct WithRunner(&'static [&'static [(&'static str, &'static str)]]);

impl SourceTexts<'static> for WithRunner {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 1).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let text = match (call.kind, def.proc_name()) {
            (ProcKind::Derive, Some("Codec")) => "",
            (ProcKind::Attr, Some("add_tests")) => "#[codec(crate = \"x\")] pub struct Entry { pub f: u8 } pub mod entry_tests {}",
            (ProcKind::Attr, Some("visited")) => "#[codec(skip)] pub struct Seen;",
            _ => return Err(refused),
        };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: WithRunner = WithRunner(&[&[("/user/lib.rs", USER)], &[("/macros/lib.rs", MACROS)]]);

/// The root's macro errors, as (path, why), sorted.
fn macro_errors(names: &CrateNames<'_>) -> Vec<(String, Unresolved)> {
    let errors = &names.done.get(&names.root).expect("the root is done").scope.errors;
    let mut out: Vec<(String, Unresolved)> = errors
        .iter()
        .filter_map(|e| match e {
            ScopeError::Macro { path, why, .. } => Some((path.segments.join("::"), *why)),
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Rule: `#[macros::visited]` written after `#[derive(Codec)]` is sent after the derive, as an
/// after-derive call (never the item's one call, which left the derive unsent); `#[codec(..)]`
/// written back by `add_tests` and by `visited` is `Codec`'s helper there too, inert; so `Entry`,
/// `entry_tests` and `Seen` are declared in the root and no macro error is written.
///
/// Expected: "1 passed, 0 failed". Before C01 it fails at "`Entry` is declared": `codec` in
/// `add_tests`' output was called as an attribute macro and failed NotFound, and `Entry` was never
/// declared. Controls: with `Cells::derive_helper`'s walk up the fragments removed, the error list
/// is `[("codec", NotFound), ("codec", NotFound)]` and neither struct is declared; with
/// `Lower::item`'s `derived_first` false, `macros::visited` is a `CallRole::Call` and the role
/// assertion fails.
#[test]
fn a_derive_helper_stays_inert_in_the_output_of_the_macro_that_took_its_item() {
    let bump = super::SyncFrame::leaked();
    let alloc = bump.alloc();
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("macros", 1)]), CrateInput { implicit, ..macros }];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let user = names.krate(0);
    let decls = &user.decls.get(&user.root).expect("the root is lowered").1;
    let visited: Vec<CallRole> =
        decls.calls.iter().filter(|c| c.kind == CallShape::Attr && c.path.segments.join("::") == "macros::visited").map(|c| c.role).collect();
    let calls: Vec<(CallShape, String, CallRole, Option<u32>, &str)> = decls
        .calls
        .iter()
        .map(|c| (c.kind, c.path.segments.join("::"), c.role, c.item, user.tables.calls.get(&c.call).map_or("", |s| s.input)))
        .collect();
    assert_eq!(visited, [CallRole::AfterDerive], "the path after the derive is one after-derive call: the root's calls {calls:?}; notes {:?}", names.notes(0));
    let at = |ns: Namespace, segments: &'static [&'static str]| names.item_at(names.root(0), ns, Written { kind: PathKind::Plain, segments }, alloc);
    assert!(at(Namespace::Types, &["Entry"]).is_ok(), "`Entry` is declared: errors {:?}; notes {:?}", macro_errors(user), names.notes(0));
    assert!(at(Namespace::Types, &["entry_tests"]).is_ok(), "`add_tests`' test module is declared");
    assert!(at(Namespace::Types, &["Seen"]).is_ok(), "`Seen` is declared: errors {:?}", macro_errors(user));
    let errors = macro_errors(user);
    assert!(errors.is_empty(), "no helper is called as a macro: {errors:?}; notes {:?}", names.notes(0));
}
