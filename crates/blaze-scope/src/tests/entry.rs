//! The one-call entry, `resolve::resolve_crates`, on small crates written here: what a later
//! stage asks of its `Names`.

use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;
use blaze_surface::{DefKind, Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use crate::graph::Sysroot;
use crate::names::{Why, Written};
use crate::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use crate::task::Serial;

/// Each crate's files, by its index.
pub(super) struct Texts(pub(super) &'static [&'static [(&'static str, &'static str)]]);

impl<'a> SourceTexts<'a> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        self.0.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }
}

const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

pub(super) fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-scope test crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: RUST,
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

/// Crates resolved with no sysroot (each root `#![no_core]`), one thread.
pub(super) fn resolved(crates: &[CrateInput<'static>], texts: &'static Texts) -> Names<'static> {
    resolve_crates(crates, Sysroot::default(), texts, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

const DEP: &str = "#![no_core]
#[macro_export]
macro_rules! made { () => {} }
pub mod a {
    pub fn open() {}
    fn shut() {}
    pub struct Kept;
    pub struct Named { x: u8 }
    pub enum Way { Up, #[cfg(nothing)] Gone, Down(u8) }
    impl Kept {
        #[cfg(nothing)]
        fn gone() {}
        pub fn new() -> Kept { Kept }
        const C: u8 = 0;
    }
}
";

const USER: &str = "#![no_core]
pub fn main() {}
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);

/// libc's shape (libc-0.2.191 src/lib.rs): a `#[macro_use] mod macros;` defines `cfg_if!`, and
/// the crate's `extern crate .. as core;` and its modules are written inside `cfg_if!` calls.
const SHAPED: &str = "#![no_core]
#[macro_use]
mod macros;
cfg_if! { extern crate dep as core; }
cfg_if! { mod unix; }
pub use core::a::open;
";

const SHAPED_MACROS: &str = "macro_rules! cfg_if { ($($t:tt)*) => { $($t)* } }
";

/// `::core::a::Named` is the derives' path form (`::core::clone::Clone`): `::` begins at the
/// extern prelude, which holds the macro-written `core` (the Reference, "Paths", "Path
/// qualifiers": `::` from 2018 begins at the extern prelude).
const SHAPED_UNIX: &str = "cfg_if! { pub fn u() {} }
pub use core::a::Kept;
pub use ::core::a::Named;
";

static SHAPED_TEXTS: Texts = Texts(&[&[("/user/lib.rs", SHAPED), ("/user/macros.rs", SHAPED_MACROS), ("/user/unix.rs", SHAPED_UNIX)], &[("/dep/lib.rs", DEP)]]);

#[test]
fn path_root_segment_is_the_extern_prelude_from_2018() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let root = names.root(0);
    let at = |segments: &'static [&'static str]| names.item_at(root, Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global());
    let open = at(&["{{root}}", "dep", "a", "open"]).unwrap_or_else(|e| panic!("`{{{{root}}}}::dep::a::open`: {e}"));
    assert_eq!(open.krate, 1, "`::dep::a::open` is dep's");
    let main = at(&["{{root}}", "main"]).map(|f| f.def);
    assert_eq!(main.map_err(|e| (e.at, e.why)), Err((1, Why::NotFound)), "`::main` names no crate in 2021");
}

/// The Reference, "Extern crate declarations" and "Extern prelude": an `extern crate` in the
/// crate root adds its name to the extern prelude, also when a macro's expansion writes it.
#[test]
fn libc_shape_macro_written_extern_crate_and_modules() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &SHAPED_TEXTS);
    let root = names.root(0);
    let at = |ns, segments: &'static [&'static str]| names.item_at(root, ns, Written { kind: PathKind::Plain, segments }, Alloc::global());
    let open = at(Namespace::Values, &["open"]).unwrap_or_else(|e| panic!("`open`, imported through the macro-written `core`: {e}"));
    assert_eq!(open.krate, 1, "`open` is dep's");
    let u = at(Namespace::Values, &["unix", "u"]).unwrap_or_else(|e| panic!("`unix::u`, written by `cfg_if!` in a macro-written module: {e}"));
    assert_eq!(u.krate, 0, "`unix::u` is user's");
    let kept = at(Namespace::Types, &["unix", "Kept"]).unwrap_or_else(|e| {
        let errors: std::vec::Vec<_> = names.krate(0).done.values().map(|d| &d.scope.errors).collect();
        panic!("`unix::Kept`, imported in the macro-written module through `core`: {e}; errors {errors:?}; notes {:?}", names.notes(0))
    });
    assert_eq!(kept.krate, 1, "`Kept` is dep's");
    let named = at(Namespace::Types, &["unix", "Named"]).unwrap_or_else(|e| panic!("`unix::Named`, imported by `::core::a::Named`: {e}"));
    assert_eq!(named.krate, 1, "`Named` is dep's");
}

#[test]
fn item_at_names_a_dependency_s_macro_and_says_why_not() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let root = names.root(0);
    let at = |ns, segments: &'static [&'static str]| names.item_at(root, ns, Written { kind: PathKind::Plain, segments }, Alloc::global());
    let made = at(Namespace::Macros, &["dep", "made"]).unwrap_or_else(|e| panic!("`dep::made`: {e}"));
    assert_eq!(made.krate, 1, "`dep::made` is dep's");
    assert_eq!(made.macro_src.map(|m| m.name), Some("made"), "`dep::made`'s source");
    let open = at(Namespace::Values, &["dep", "a", "open"]).unwrap_or_else(|e| panic!("`dep::a::open`: {e}"));
    assert_eq!((open.krate, open.def.kind, open.macro_src.is_none()), (1, DefKind::Other, true), "`dep::a::open`");
    let shut = at(Namespace::Values, &["dep", "a", "shut"]).map(|f| f.def);
    assert_eq!(shut.map_err(|e| (e.at, e.why)), Err((2, Why::Private)), "`dep::a::shut` is private to dep");
    let none = at(Namespace::Macros, &["dep", "unmade"]).map(|f| f.def);
    assert_eq!(none.map_err(|e| (e.at, e.why)), Err((1, Why::NotFound)), "`dep::unmade`");
    // A segment before the last is a type: `open` is a value, so not found there.
    let through_fn = at(Namespace::Values, &["dep", "a", "open", "more"]).map(|f| f.def);
    assert_eq!(through_fn.map_err(|e| (e.at, e.why)), Err((2, Why::NotFound)), "`dep::a::open::more`");
    let assoc = at(Namespace::Values, &["dep", "a", "Kept", "new"]).map(|f| f.def);
    assert_eq!(assoc.map_err(|e| (e.at, e.why)), Err((3, Why::NotAModule)), "`dep::a::Kept::new` is the type checker's");
}

#[test]
fn members_ctors_and_variants_are_as_written_under_the_cfg() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let root = names.root(0);
    let def = |segments: &'static [&'static str]| {
        names.item_at(root, Namespace::Types, Written { kind: PathKind::Plain, segments }, Alloc::global()).unwrap_or_else(|e| panic!("{segments:?}: {e}")).def.key
    };
    let kept = def(&["dep", "a", "Kept"]);
    assert_eq!(names.ctor(kept), Some(kept), "a unit struct is its own constructor");
    assert_eq!(names.ctor(def(&["dep", "a", "Named"])), None, "a struct with named fields has none");
    let way: Vec<&str> = names.variants(def(&["dep", "a", "Way"])).unwrap_or(&[]).iter().map(|v| v.name).collect();
    assert_eq!(way, ["Up", "Down"], "variants in order, the cfg'd-out one gone");
    let impls = &names.krate(1).tables.impls;
    assert_eq!(impls.len(), 1, "dep's one impl");
    let members: Vec<Option<&str>> = names.members(impls[0].key).unwrap_or(&[]).iter().map(|m| m.name).collect();
    assert_eq!(members, [Some("new"), Some("C")], "the impl's members in order, the cfg'd-out one gone");
}

/// Reduced from the pinned library's core/src/num/mod.rs (`impl u32 { uint_impl! { Self = u32,
/// BITS = 32, MAX = 4294967295, .. } }`, whose transcription writes `pub const BITS`,
/// `pub const MAX` and the methods), a trait whose provided methods a call writes, and a call
/// that writes another call.
const MEMBERS_BY_CALLS: &str = "#![no_core]
macro_rules! uint_impl {
    (Self = $SelfT:ty, BITS = $BITS:literal, MAX = $MaxV:literal,) => {
        pub const MIN: Self = 0;
        pub const MAX: Self = $MaxV;
        pub const BITS: u32 = $BITS;
        pub const fn count_ones(self) -> u32 { 0 }
        more_impl! { $SelfT }
    };
}
macro_rules! more_impl { ($T:ty) => { pub const fn is_zero(self) -> bool { true } }; }
macro_rules! provided { () => { fn hello(&self) {} fn bye(&self) {} }; }
pub struct U;
impl U {
    uint_impl! { Self = U, BITS = 32, MAX = 4294967295, }
    pub fn written() {}
}
pub trait Greet { fn name(&self); provided!(); }
";

static MEMBERS_TEXTS: Texts = Texts(&[&[("/m/lib.rs", MEMBERS_BY_CALLS)]]);

/// The Reference, "Macro invocation": "A macro invocation expands a macro at compile time and
/// replaces the invocation with the result of the macro. Macros may be invoked in the following
/// situations: [..] Items including associated items". A call in an `impl` or `trait` body is
/// expanded through the same hook as a module's, and each associated item it writes (a call it
/// writes in turn too) is the owner's member, keyed by owner, kind and name, carrying the call
/// that wrote it.
#[test]
fn a_macro_call_in_an_impl_or_trait_body_writes_its_members() {
    let crates = [input("m", "/m/lib.rs", &[])];
    let names = resolved(&crates, &MEMBERS_TEXTS);
    let impls = &names.krate(0).tables.impls;
    assert_eq!(impls.len(), 1, "one impl");
    let members = names.members(impls[0].key).unwrap_or(&[]);
    let mut named: Vec<&str> = members.iter().filter_map(|m| m.name).collect();
    named.sort_unstable();
    assert_eq!(named, ["BITS", "MAX", "MIN", "count_ones", "is_zero", "written"], "every member the calls wrote, beside the written one");
    let by = |name: &str| members.iter().find(|m| m.name == Some(name)).map(|m| m.written_by);
    assert_eq!(by("written"), Some(None), "a written member names no call");
    let (max, zero) = (by("MAX").flatten(), by("is_zero").flatten());
    assert!(max.is_some() && zero.is_some() && max != zero, "each member names the call that wrote it: {max:?} {zero:?}");
    let root = names.root(0);
    let greet = names
        .item_at(root, Namespace::Types, Written { kind: PathKind::Plain, segments: &["Greet"] }, Alloc::global())
        .unwrap_or_else(|e| panic!("`Greet`: {e}"))
        .def
        .key;
    let mut provided: Vec<&str> = names.members(greet).unwrap_or(&[]).iter().filter_map(|m| m.name).collect();
    provided.sort_unstable();
    assert_eq!(provided, ["bye", "hello", "name"], "the trait's provided methods a call wrote");
}

/// serde_core's shape (serde_core-1.0.229 src/macros.rs): `forward_to_deserialize_any!` is
/// `#[macro_export(local_inner_macros)]` and writes `forward_to_deserialize_any_helper!{..}`
/// calls, single-name, in an `impl Deserializer` body of a crate that never names the helper.
const INNER_DEP: &str = "#![no_core]
#[macro_export(local_inner_macros)]
macro_rules! outer { ($($f:ident)*) => { $(helper!{$f})* } }
#[doc(hidden)]
#[macro_export]
macro_rules! helper { ($f:ident) => { fn $f() {} } }
";

const INNER_USER: &str = "#![no_core]
use dep::outer;
pub struct S;
impl S { outer! { a b } }
";

static INNER_TEXTS: Texts = Texts(&[&[("/user/lib.rs", INNER_USER)], &[("/dep/lib.rs", INNER_DEP)]]);

/// The Reference, "Macros By Example", "Hygiene": "Additionally, the `local_inner_macros` key
/// can be used to automatically prefix all contained macro invocations with `$crate::`." The
/// helper a `local_inner_macros` macro's body calls by one name is the defining crate's, found
/// from the calling crate where no such name is in scope, and the members it writes are the
/// impl's.
#[test]
fn a_local_inner_macros_body_calls_its_own_crate_s_macros() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &INNER_TEXTS);
    let impls = &names.krate(0).tables.impls;
    assert_eq!(impls.len(), 1, "one impl");
    let mut named: Vec<&str> = names.members(impls[0].key).unwrap_or(&[]).iter().filter_map(|m| m.name).collect();
    named.sort_unstable();
    assert_eq!(named, ["a", "b"], "the members the defining crate's helper wrote");
}

/// The pinned library's shape (alloc's `use core::str;` beside `&str` and `str::from_utf8`): a
/// module named `str` in scope beside the built-in type `str`.
const STR_DEP: &str = "#![no_core]
pub mod str { pub fn from_utf8() {} }
";

const STR_USER: &str = "#![no_core]
use dep::str;
pub fn f(_: &str) { str::from_utf8(); }
pub mod own {
    pub struct str;
    pub fn g(_: &str) {}
}
";

static STR_TEXTS: Texts = Texts(&[&[("/user/lib.rs", STR_USER)], &[("/dep/lib.rs", STR_DEP)]]);

/// The Reference, "Preludes", "Language prelude": "The language prelude includes names of types
/// and attributes that are built-in to the language. The language prelude is always in scope."
/// and "Scopes", "Prelude scopes": "The prelude names may be shadowed by declarations in a
/// module." It says nothing of a module of a built-in type's name; the pinned library writes
/// `use core::str;` and then `&str` and `str::from_utf8(..)` in one file, so a whole one-segment
/// path in type position whose lexical binding is a MODULE is the built-in type (a module is never
/// a type), while as a path's first segment the module stands; a struct of the name shadows the
/// built-in as the Reference says.
#[test]
fn builtin_type_name_beside_a_module_of_its_name() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &STR_TEXTS);
    let root = names.root(0);
    let path = |module, ns, segments: &'static [&'static str]| {
        names.path(module, ns, Written { kind: PathKind::Plain, segments }, Alloc::global()).unwrap_or_else(|e| panic!("{segments:?}: {e}"))
    };
    let ty = path(root, Namespace::Types, &["str"]);
    assert_eq!((ty.def, ty.consumed), (crate::builtin::builtin_type("str").expect("`str` is built in"), 1), "`&str` beside `use dep::str;`");
    let call = path(root, Namespace::Values, &["str", "from_utf8"]);
    assert_eq!(call.consumed, 2, "`str::from_utf8` walks through the module");
    assert_eq!(names.crate_of(call.def.key), Some(1), "`str::from_utf8` is dep's fn");
    let own = names.item_at(root, Namespace::Types, Written { kind: PathKind::Plain, segments: &["own"] }, Alloc::global()).unwrap_or_else(|e| panic!("`own`: {e}"));
    let shadow = path(own.def.key, Namespace::Types, &["str"]);
    assert_eq!((shadow.def.kind, names.crate_of(shadow.def.key)), (DefKind::Other, Some(0)), "`struct str;` shadows the built-in");
    assert_ne!(Some(shadow.def), crate::builtin::builtin_type("str"), "the local struct, not the built-in");
}

/// The `uint` crate's `#[doc(hidden)] pub use core as core_;` and its `construct_uint!`
/// writing `$crate::core_::ops::Mul` (expanded in discv5): a `pub use` of an extern-prelude
/// crate under a new name is a public re-export (the Reference, "Use declarations", "Use
/// visibility"), so `uint::core_::ops::Mul` resolves from any crate, from the user's own text and
/// from the expansion of uint's macro alike. The `extern crate base;` stands for the `extern crate
/// core;` every root is given (private): `base` names that item first, and a single `use` of an
/// extern crate item keeps its own visibility (rustc's `pub_use_of_private_extern_crate_hack` in
/// `Resolver::import`; at most a lint, never a narrower binding).
const UINT: &str = "#![no_core]
extern crate base;
#[doc(hidden)]
pub use base as base_;
#[macro_export]
macro_rules! construct {
    () => { pub use $crate::base_::ops::Mul as Made; };
}
";
const BASE: &str = "#![no_core]
pub mod ops { pub trait Mul {} }
";
const UINT_USER: &str = "#![no_core]
uint::construct!();
";
static UINT_TEXTS: Texts = Texts(&[&[("/user/lib.rs", UINT_USER)], &[("/uint/lib.rs", UINT)], &[("/base/lib.rs", BASE)]]);

#[test]
fn a_pub_use_of_a_crate_renamed_is_a_public_re_export() {
    let crates = [input("user", "/user/lib.rs", &[("uint", 1)]), input("uint", "/uint/lib.rs", &[("base", 2)]), input("base", "/base/lib.rs", &[])];
    let names = resolved(&crates, &UINT_TEXTS);
    let root = names.root(0);
    let at = |segments: &'static [&'static str]| names.item_at(root, Namespace::Types, Written { kind: PathKind::Plain, segments }, Alloc::global());
    let plain = at(&["uint", "base_", "ops", "Mul"]).unwrap_or_else(|e| panic!("`uint::base_::ops::Mul`: {e}"));
    assert_eq!(plain.krate, 2, "`uint::base_::ops::Mul` is base's");
    let made = at(&["Made"]).unwrap_or_else(|e| {
        let errors: Vec<_> = names.krate(0).done.values().map(|d| &d.scope.errors).collect();
        panic!("`Made`, imported by uint's macro through `$crate::base_`: {e}; errors {errors:?}")
    });
    assert_eq!(made.krate, 2, "`Made` is base's `Mul`");
}

/// Rule 3: a crate resolved while a dependency it names is not finished is refused by name
/// (`NamesError::DepNotFinished`), never resolved with that name left out of its root. The
/// wrong version (the dependency dropped from the extern prelude) resolves `user` and fails
/// here with `resolved anyway`. Passes printing nothing.
#[test]
fn a_dependency_not_finished_is_refused_by_name() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let graph = crate::resolve::graph_of(&crates, Sysroot::default(), alloc).unwrap_or_else(|e| panic!("the graph: {e}"));
    // Nothing finished: `dep` is not resolved before `user`.
    let finished: [Option<crate::crate_scope::Dep<'_>>; 2] = [None, None];
    let by_prefix = blaze_hash::HashMap::new_in(alloc);
    let built = crate::resolve::Built { crates: &finished, by_prefix: &by_prefix, asked: None };
    let got = crate::resolve::resolve_one(
        &graph,
        &crates[0],
        crate::graph::CrateId(0),
        0,
        &built,
        &TEXTS,
        &Serial,
        crate::task::First::Prefetch,
        crate::prepares::Prepares::new_in(bump),
        &crate::expansions::KeptExpansions::none(alloc),
        bump,
    );
    match got {
        Err(crate::resolve::NamesError::DepNotFinished { krate, name, dep }) => assert_eq!((krate, name, dep), ("user", "dep", "dep")),
        Err(e) => panic!("another refusal: {e}"),
        Ok(_) => panic!("resolved anyway: `dep` left out of user's root"),
    }
}
