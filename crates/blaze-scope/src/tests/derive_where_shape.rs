//! BM (2026-11-17): derive_where's whole shape, two levels deep, as tempo's evm2 writes it.
//!
//! tempo's build census g033201: 1,183 left-out functions sit behind one impl,
//! `evm2::evm::state::account::<impl Drop for AccountHandle<'_, '_>>`, whose self type is NotFound
//! (and 65 more behind `PoolResults<T>` and `Interpreter<..>`): all three structs are written
//! `#[derive_where(Debug)] pub struct ..` with `use derive_where::derive_where;` in scope. The
//! expansions rustc makes (`take_first_attr`, derive-where 1.6/1.7 src/lib.rs):
//! 1. `#[derive_where(Debug)]` (the attribute macro) writes `#[derive(::derive_where::DeriveWhere)]
//!    #[derive_where(Debug)] <the docs> #[::derive_where::derive_where_visited] pub struct ..`;
//! 2. there the derive runs first; `#[derive_where(..)]` after it is its helper (inert, though an
//!    attribute macro of that name is imported); then the no-op attribute macro
//!    `derive_where_visited` takes the item, as the derive saw it, and writes it back with
//!    `#[derive_where(Debug)]` still on it;
//! 3. in that output the name is still the helper (`Scope::DeriveHelpers` of the parent
//!    expansion), so the item is kept and the struct declared.
//!
//! Were `derive_where` in (3) called as the attribute macro, it would write (1) again, forever,
//! up to the recursion limit, and the struct would never be declared: the census's
//! `derive_where ... ran; declares AccountHandle` rows, some 200 for one struct.

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
use crate::resolve::{CrateNames, SourceTexts, resolve_crates};
use crate::task::Serial;
use crate::CallShape;

/// derive-where's three exports, declared as the crate declares them.
const MACROS: &str = "#![no_core]
#[proc_macro_attribute]
pub fn derive_where(attr: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_derive(DeriveWhere, attributes(derive_where))]
pub fn derive_where_actual(item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn derive_where_visited(attr: TokenStream, item: TokenStream) -> TokenStream { item }
";

const ROOT: &str = "#![no_core]
pub mod account;
pub mod stack;
";

/// evm2's src/evm/state/account.rs, reduced: the struct under `#[derive_where(Debug)]`, a field
/// helper, and an impl written at module level after it.
const ACCOUNT: &str = "use derive_where::derive_where;
pub trait Show {}
pub trait Drop {}
/// A handle.
#[derive_where(Debug)]
pub struct Handle<'a> {
    a: &'a u8,
    #[derive_where(skip)]
    b: &'a u8,
}
impl Drop for Handle<'_> {}
";

/// evm2's src/interpreter/stack.rs, reduced: two bodies whose blocks declare items of equal text.
const STACK: &str = "pub fn first() { const _: () = (); struct Inner; }
pub fn second() { const _: () = (); struct Inner; }
";

/// What derive-where writes for `Handle`: the attribute macro, then the derive (an impl of
/// `Show`, the stand-in for `Debug`), then the visited no-op (its input: the item as the derive
/// saw it, `#[derive_where(Debug)]` still on it).
const BY_ATTR: &str = "#[derive(::derive_where::DeriveWhere)] #[derive_where(Debug)] #[doc = \" A handle.\"] #[::derive_where::derive_where_visited] pub struct Handle<'a> { a: &'a u8, #[derive_where(skip)] b: &'a u8, }";
const BY_DERIVE: &str = "impl<'a> Show for Handle<'a> {}";
const BY_VISITED: &str = "#[derive_where(Debug)] #[doc = \" A handle.\"] pub struct Handle<'a> { a: &'a u8, #[derive_where(skip)] b: &'a u8, }";

const RUNNER: CrateItemKey = CrateItemKey(ContentId([11; 32]));

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
            (ProcKind::Attr, Some("derive_where")) => BY_ATTR,
            (ProcKind::Derive, Some("DeriveWhere")) => BY_DERIVE,
            (ProcKind::Attr, Some("derive_where_visited")) => BY_VISITED,
            _ => return Err(refused),
        };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: WithRunner = WithRunner(&[&[("/user/lib.rs", ROOT), ("/user/account.rs", ACCOUNT), ("/user/stack.rs", STACK)], &[("/derive_where/lib.rs", MACROS)]]);

/// Every module's scope errors, as text, sorted.
fn errors(names: &CrateNames<'_>) -> Vec<String> {
    let mut out: Vec<String> = names.done.values().flat_map(|d| d.scope.errors.iter()).map(|e| std::format!("{e:?}")).collect();
    out.sort();
    out
}

/// Rule: in derive_where's first output the derive is sent, then `derive_where` and the path
/// `::derive_where::derive_where_visited` as after-derive calls (a path after a derive is an
/// attribute macro, never a built-in attribute), then the keep call; so the derive runs, the
/// helper stays inert there and in `derive_where_visited`'s output, the attribute macro
/// `derive_where` runs once, and `Handle` is declared in `account`, where the written impl's
/// header names it, beside the derive's impl, with no call in error.
///
/// Expected: "1 passed, 0 failed". On green 1224a76dd6 it fails at the roles: the output filed
/// one call, `derive_where::derive_where_visited` as `CallRole::Call` (`Cfg::attr` kept a path out
/// of `others`: `is_builtin_attr` is true of every path), the derive never ran, `derive_where`
/// was filed 65 times, every other level called as the macro, down to two `RecursionLimit` errors, and `Handle` was NotFound.
#[test]
fn derive_where_s_struct_is_declared_through_its_two_expansions() {
    let bump = super::SyncFrame::leaked();
    let macros = input("derive_where", "/derive_where/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("derive_where", 1)]), crate::resolve::CrateInput { implicit, ..macros }];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let user = names.krate(0);
    // The calls the first expansion (the attribute macro's, one deep) files, in order.
    let first: Vec<(CallShape, String, CallRole)> = user
        .expansions
        .iter()
        .filter(|&(k, _)| user.tables.calls.get(k).is_some_and(|c| c.depth == 0))
        .flat_map(|(_, e)| e.decls.calls.iter().map(|c| (c.kind, c.path.segments.join("::"), c.role)))
        .collect();
    // The `derive_where` attribute calls that made text: run as the macro, not inert as the helper.
    let attr_runs = user
        .tables
        .calls
        .iter()
        .filter(|&(k, c)| c.kind == CallShape::Attr && c.name.trim() == "derive_where" && user.expansions.get(k).is_some_and(|e| e.made.is_some()))
        .count();
    assert_eq!(
        first,
        [
            (CallShape::Derive, String::from("derive_where::DeriveWhere"), CallRole::Call),
            (CallShape::Attr, String::from("derive_where"), CallRole::AfterDerive),
            (CallShape::Attr, String::from("derive_where::derive_where_visited"), CallRole::AfterDerive),
            (CallShape::Attr, String::from("derive"), CallRole::Keep),
        ],
        "the first output's calls; `derive_where` attribute calls filed in all: {attr_runs}; errors {:?}",
        errors(user)
    );
    let at = |segments: &'static [&'static str]| names.item_at(names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments }, bump.alloc());
    let account = at(&["account"]).unwrap_or_else(|e| panic!("`account`: {e}")).def.key;
    let handle = names.path(account, Namespace::Types, Written { kind: PathKind::Plain, segments: &["Handle"] }, bump.alloc());
    assert!(handle.is_ok(), "`Handle` resolves in `account` (the impl header's path): {:?}; errors {:?}", handle.err(), errors(user));
    assert_eq!(attr_runs, 1, "the attribute macro `derive_where` runs once");
    assert_eq!(errors(user), Vec::<String>::new(), "no call in error");
    assert_eq!(user.tables.impls.len(), 2, "the derive's impl and the written one");
    // The other file's bodies declare items of equal text in their blocks: each body's block
    // module is its own, and neither is the module an impl of `account` is read in.
    let stack = at(&["stack"]).unwrap_or_else(|e| panic!("`stack`: {e}")).def.key;
    assert_ne!(stack, account, "two modules");
}
