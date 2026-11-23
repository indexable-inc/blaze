//! Body macro calls resolved through the names library ([`crate::scoped::Scoped`]) over two
//! crates, `user` depending on `dep`, resolved by blaze-scope's `resolve_crates`.
//!
//! The rule under test (the Reference, "Macros By Example", Hygiene): an identifier in a
//! macro expansion is looked up as if it appeared at the invocation site, except `$crate`,
//! which names the crate where the macro was defined; a `macro_rules!` in textual scope
//! (defined earlier in the body) shadows one reached by path-based scope.

use std::string::String;
use std::vec::Vec as StdVec;

use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{Alloc, Bump};
use blaze_scope::graph::Sysroot;

fn test_bump() -> Bump<'static> {
    blaze_own::testing::bump()
}
use blaze_scope::resolve::{CrateInput, MacroDefError, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use crate::by_name::{ByName, NoMacro};
use crate::expand_tests::{Host, Unix, dump_of};
use crate::lower_body_expanding;
use crate::scoped::{Prepares, Scoped};

const DEP: &str = "#![no_core]
#[macro_export]
macro_rules! m { ($e:expr) => { $e } }
#[macro_export]
macro_rules! seven_of { () => { $crate::inner::SEVEN } }
#[macro_export]
macro_rules! via { () => { $crate::m!(3) } }
pub mod inner { pub const SEVEN: u32 = 7; }
// A nested pair, the shape of core's `panic!` -> `panic_2021!` -> `$crate::panicking::panic_fmt`.
#[macro_export]
macro_rules! outer { () => { $crate::inner_m!() } }
#[macro_export]
macro_rules! inner_m { () => { $crate::deep::f() } }
pub mod deep { pub fn f() -> u32 { 1 } }
// core's `const_eval_select`: core panic.rs `const_panic!` writes `$crate::intrinsics::const_eval_select!(..)`,
// a `macro` item of core::intrinsics whose body calls the bare `const_eval_select(..)` fn of that module.
pub mod intr {
    pub fn sel() -> u32 { 9 }
    pub macro cev() { sel() }
}
#[macro_export]
macro_rules! cpanic { () => { $crate::intr::cev!() } }
// The control: a `macro_rules!` writing bare item names. `deep::f` is in both `dep` and `user`;
// `only_dep` only in `dep`.
pub fn only_dep() -> u32 { 4 }
#[macro_export]
macro_rules! call_deep { () => { deep::f() } }
#[macro_export]
macro_rules! call_only_dep { () => { only_dep() } }
// core's `extern crate self as core;` and `assert_unsafe_precondition!`'s `::core::ub_checks::$kind()`.
extern crate self as dep;
macro_rules! kind_call { ($kind:ident) => { ::dep::deep::$kind() } }
pub mod again { pub use crate::m as re; }
pub mod globbed { pub use crate::seven_of; }

// alloy-primitives 1.7.3 (hex_literal.rs, bits/macros.rs): `hex!` is exported, and
// `fixed_bytes_macros!` WRITES the exported `address!` whose body calls `$crate::hex!`.
#[macro_export]
macro_rules! hex { ($($s:literal)*) => { $crate::inner::SEVEN } }
macro_rules! fixed_bytes_macros {
    ($d:tt $(macro $name:ident($ty:ident);)*) => {$(
        #[macro_export]
        macro_rules! $name {
            () => { $crate::$ty::SEVEN };
            ($d ($d t:tt)+) => { $crate::hex!($d ($d t)+) };
        }
    )*};
}
fixed_bytes_macros! { $
    macro address(inner);
}

// antithesis_sdk 0.3.0 (assert/macros.rs): `assert_sometimes_greater_than!` writes
// `$crate::numeric_guidance_helper!($crate::assert_sometimes, ..)`, whose transcriber calls the
// macro its `$assert:path` fragment names: `$assert!(..)`.
#[macro_export]
macro_rules! guided { ($assert:path, $e:expr) => { $assert!($e) } }
#[macro_export]
macro_rules! sometimes { ($e:expr) => { $crate::guided!($crate::m, $e) } }

// tokio 1.53.2 (macros/try_join.rs): `doc!` adds `#[macro_export]` to the `macro_rules!` item it
// is handed, and that macro calls itself again through `$crate::try_join!(@ ..)`.
macro_rules! doc { ($try_join:item) => { #[macro_export] $try_join }; }
doc! {macro_rules! try_join {
    (@ { $($t:tt)* } $e:expr) => { $e };
    ($e:expr) => { $crate::try_join!(@ { } $e) };
}}

// The pinned library's `panic!` family, as `core` writes it (macros/mod.rs, panic.rs).
#[macro_export]
#[rustc_builtin_macro(core_panic)]
macro_rules! panic { ($($arg:tt)*) => { /* compiler built-in */ } }
#[macro_export]
#[rustc_builtin_macro]
macro_rules! const_format_args { ($fmt:expr) => {{}}; ($fmt:expr, $($args:tt)*) => {{}}; }
#[macro_export]
#[rustc_builtin_macro]
macro_rules! option_env { ($name:expr $(,)?) => {{}} }
#[macro_export]
#[rustc_builtin_macro]
macro_rules! include_str { ($file:expr $(,)?) => {{}} }
#[macro_export]
#[rustc_builtin_macro]
macro_rules! include_bytes { ($file:expr $(,)?) => {{}} }
#[macro_export]
#[rustc_builtin_macro]
macro_rules! include { ($file:expr $(,)?) => {{}} }
#[macro_export]
#[rustc_builtin_macro]
macro_rules! env { ($name:expr $(,)?) => {{}}; ($name:expr, $error_msg:expr $(,)?) => {{}}; }
#[macro_export]
#[rustc_builtin_macro]
macro_rules! concat { ($($arg:expr),* $(,)?) => {{}} }
pub mod panicking {
    pub fn panic(_: &str) {}
    pub fn panic_str_2015(_: &str) {}
    pub fn panic_display<T>(_: &T) {}
    pub fn panic_fmt<T>(_: T) {}
    pub fn panic_default_msg() {}
}
pub mod panic {
    pub macro panic_2015 {
        () => ($crate::panicking::panic_default_msg()),
        ($msg:literal $(,)?) => ($crate::panicking::panic($msg)),
        ($msg:expr $(,)?) => ({ $crate::panicking::panic_str_2015($msg); }),
        (\"{}\", $arg:expr $(,)?) => ({ $crate::panicking::panic_display(&$arg); }),
        ($fmt:expr, $($arg:tt)+) => ({ $crate::panicking::panic_fmt($crate::const_format_args!($fmt, $($arg)+)); }),
    }
    pub macro panic_2021 {
        () => ($crate::panicking::panic_default_msg()),
        (\"{}\", $arg:expr $(,)?) => ({ $crate::panicking::panic_display(&$arg); }),
        ($($t:tt)+) => ({ $crate::panicking::panic_fmt($crate::const_format_args!($($t)+)); }),
    }
}
";

const OLD_2015: &str = "#![no_core]
pub fn main() {}
#[macro_export]
macro_rules! fail { () => { dep::panic!(\"{}\") } }
";

const USER: &str = "#![no_core]
use dep::seven_of;
mod g { pub use dep::globbed::*; }
mod deep { pub fn f() -> u32 { 2 } }
pub fn main() {}
";

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (0, "/user/lib.rs") => Some(USER),
            (1, "/dep/lib.rs") => Some(DEP),
            (2, "/old/lib.rs") => Some(OLD_2015),
            _ => None,
        }
    }
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)], edition: Edition) -> CrateInput<'static> {
    let prelude = match edition {
        Edition::E2015 => edition::Edition::Edition2015,
        _ => edition::Edition::Edition2021,
    };
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-lower scoped test crate", name.as_bytes())),
        edition,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(prelude), proc_macro: false, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

/// `user` (crate 0, 2021) and `old` (crate 2, 2015) over `dep` (crate 1); `user` also sees `old`.
fn names() -> &'static Names<'static> {
    let crates = [
        input("user", "/user/lib.rs", &[("dep", 1), ("old", 2)], Edition::E2021),
        input("dep", "/dep/lib.rs", &[], Edition::E2021),
        input("old", "/old/lib.rs", &[("dep", 1)], Edition::E2015),
    ];
    let names = resolve_crates(&crates, Sysroot::default(), &Texts, &Serial, test_bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    std::boxed::Box::leak(std::boxed::Box::new(names))
}

/// `text` lowered in `user`'s root module: the dump, the unhandled forms' count and the
/// findings as `(path, range start, why)`.
fn lowered(text: &str) -> (String, usize, StdVec<(String, u32, String)>) {
    lowered_in(0, text)
}

/// [`lowered`] in crate `krate`'s root module, at that crate's edition (the unit graph's).
fn lowered_in(krate: usize, text: &str) -> (String, usize, StdVec<(String, u32, String)>) {
    let names = names();
    let scoped = Scoped { names, module: names.root(krate), prepared: Some(Prepares::new_in(test_bump())) };
    let mut expander = ByName::with_names(&scoped, Host, Alloc::global());
    let l = lower_body_expanding(text, names.edition(krate), &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()).unwrap_or_else(|need| panic!("{text}: needs {:?}", need.atoms));
    let findings = expander.findings().iter().map(|f| (String::from(&*f.path), f.at.start, std::format!("{}", f.why))).collect();
    (dump_of(&l), l.view().unhandled().count(), findings)
}

#[track_caller]
fn assert_clean(text: &str, fragment: &str) {
    let (dump, unhandled, findings) = lowered(text);
    assert_eq!((unhandled, findings.len()), (0, 0), "{text}: {findings:?}\n{dump}");
    assert!(dump.contains(fragment), "{text}: `{fragment}` not in\n{dump}");
}

#[test]
fn a_dependency_s_macro_by_path() {
    assert_clean("fn a() -> u32 { dep::m!(41) }", "41");
}

#[test]
fn a_macro_export_reached_by_use_and_its_dollar_crate_item() {
    // `$crate::inner::SEVEN`: the path stays for the resolver, its first segment `$crate`.
    assert_clean("fn b() -> u32 { seven_of!() }", "SEVEN");
}

#[test]
fn a_macro_reached_through_a_glob() {
    assert_clean("fn c() -> u32 { g::seven_of!() }", "SEVEN");
}

#[test]
fn a_re_exported_macro() {
    assert_clean("fn d() -> u32 { dep::again::re!(42) }", "42");
}

#[test]
fn dollar_crate_in_an_expansion_names_the_defining_crate() {
    // `via!` writes `$crate::m!(3)`: `user` has no `m`, so only `dep`'s can expand it.
    assert_clean("fn e() -> u32 { dep::via!() }", "3");
}

/// The Reference, "Macros By Example", Hygiene: `$crate` names the crate that DEFINES the
/// macro, through every nesting: here `address!` is written by `dep`'s own
/// `fixed_bytes_macros!` (alloy-primitives' shape), so the `$crate::hex!` in its body is
/// `dep::hex!` even when `user` calls it, and `user` has no `hex` of its own.
#[test]
fn dollar_crate_in_a_macro_written_by_a_macro_names_the_defining_crate() {
    assert_clean("fn h() -> u32 { dep::address!(\"0x01\") }", "SEVEN");
    assert_clean("fn z() -> u32 { dep::address!() }", "SEVEN");
}

/// The Reference, "Macros By Example", Hygiene: "`$crate` ... refers to the crate defining the
/// macro". A value path a SECOND-level expansion writes in a body (`dep::outer!` writes
/// `$crate::inner_m!()`, which writes `$crate::deep::f()`: the pinned core's `panic!` ->
/// `$crate::panic::panic_2021!` -> `$crate::panicking::panic_fmt(..)`) resolves to `dep`'s
/// `deep::f` from `user`'s body, as the ladder's resolver asks it (`to_path` at the body's crate,
/// then the names library at the body's module), never to `user`'s own `deep::f`.
#[test]
fn a_value_path_a_nested_expansion_writes_names_the_defining_crate() {
    let (asked, dump) = asked_paths(0, "fn k() -> u32 { dep::outer!() }");
    let dep_f = item_of(1, &["deep", "f"]);
    assert!(asked.iter().any(|(_, hit)| *hit == Some(dep_f)), "{asked:?} (dep's `deep::f` is {dep_f:?})\n{dump}");
}

/// The pinned core names itself through the extern prelude (`extern crate self as core;`, core
/// lib.rs), and `assert_unsafe_precondition!` (core ub_checks.rs) writes `::core::ub_checks::
/// $kind()` and `::core::panicking::panic_nounwind_fmt(..)` in bodies of core itself: a `::dep`
/// path in a body of `dep` (which has `extern crate self as dep;`) resolves to `dep`'s item.
#[test]
fn a_global_path_through_the_crate_s_own_alias_in_its_body() {
    let (asked, dump) = asked_paths(1, "fn q() -> u32 { ::dep::deep::f() }");
    let dep_f = item_of(1, &["deep", "f"]);
    assert!(asked.iter().any(|(_, hit)| *hit == Some(dep_f)), "{asked:?}\n{dump}");
}

/// `::core::ub_checks::$kind()`: an `ident` fragment as a path's last segment, written by a macro
/// of `dep` called in `dep`'s own body through `::dep`.
#[test]
fn an_ident_fragment_as_a_path_segment_in_a_body() {
    let (asked, dump) = asked_paths(1, "fn s() -> u32 { kind_call!(f) }");
    let dep_f = item_of(1, &["deep", "f"]);
    assert!(asked.iter().any(|(_, hit)| *hit == Some(dep_f)), "{asked:?}\n{dump}");
}

/// The Reference, "Macros By Example" / `macro` items are def-site (rustc's opaque
/// transparency; drive.md "A token keeps its mark"): the bare `sel()` a `macro` of `dep::intr`
/// writes resolves at `dep::intr`, where the macro is DEFINED, from `user`'s body (which has
/// no `sel`). Without the mark (resolving at the call, as before the fix) the asked `sel` hits
/// nothing. Expected: `test scoped_tests::a_bare_name_in_a_macro_body_resolves_at_its_definition ... ok`.
#[test]
fn a_bare_name_in_a_macro_body_resolves_at_its_definition() {
    let (asked, dump) = asked_paths(0, "fn t() -> u32 { dep::intr::cev!() }");
    let sel = item_of(1, &["intr", "sel"]);
    assert!(asked.iter().any(|(p, hit)| p == "sel" && *hit == Some(sel)), "{asked:?} (dep's `intr::sel` is {sel:?})\n{dump}");
}

/// The pinned core's shape (library/core/src/panic.rs `const_panic!` writes
/// `$crate::intrinsics::const_eval_select!(..)`; library/core/src/intrinsics/mod.rs
/// `pub macro const_eval_select` writes the bare `const_eval_select(..)`, a fn of
/// `core::intrinsics`): a `macro_rules!` reaches a `macro` by `$crate`, whose bare name then
/// resolves at the `macro`'s module. Expected:
/// `test scoped_tests::the_const_eval_select_shape_resolves_at_the_macro_s_module ... ok`.
#[test]
fn the_const_eval_select_shape_resolves_at_the_macro_s_module() {
    let (asked, dump) = asked_paths(0, "fn u() -> u32 { dep::cpanic!() }");
    let sel = item_of(1, &["intr", "sel"]);
    assert!(asked.iter().any(|(p, hit)| p == "sel" && *hit == Some(sel)), "{asked:?} (dep's `intr::sel` is {sel:?})\n{dump}");
}

/// THE CONTROL (main's caution: only `macro` items are opaque). The Reference, "Macros By
/// Example", Hygiene: "By default, all identifiers referred to in a macro expansion are
/// looked up as if they appeared at the invocation site"; a `macro_rules!` is def-site only
/// for locals, labels and `$crate`. So `call_deep!` (in `dep`) writing `deep::f()` names
/// `user`'s `deep::f` when `user` calls it, never `dep`'s; and `call_only_dep!` writing
/// `only_dep()` names nothing from `user` (E0425 at the call). A resolver that read a
/// `macro_rules!` name at its definition fails both asserts. Expected:
/// `test scoped_tests::an_item_name_a_macro_rules_writes_resolves_at_the_call ... ok`.
#[test]
fn an_item_name_a_macro_rules_writes_resolves_at_the_call() {
    let (asked, dump) = asked_paths(0, "fn v() -> u32 { dep::call_deep!() }");
    let (user_f, dep_f) = (item_of(0, &["deep", "f"]), item_of(1, &["deep", "f"]));
    assert!(asked.iter().any(|(p, hit)| p == "deep::f" && *hit == Some(user_f)), "{asked:?} (user's {user_f:?}, dep's {dep_f:?})\n{dump}");
    assert!(!asked.iter().any(|(_, hit)| *hit == Some(dep_f)), "{asked:?}\n{dump}");
    let (asked, dump) = asked_paths(0, "fn w() -> u32 { dep::call_only_dep!() }");
    assert!(asked.iter().any(|(p, hit)| p == "only_dep" && hit.is_none()), "{asked:?}\n{dump}");
}

/// The library item `segments` name from crate `krate`'s root (`crate::..`).
fn item_of(krate: usize, segments: &[&str]) -> blaze_id::ItemKey {
    use blaze_scope::names::{self, Written};
    use blaze_scope::{Namespace, PathKind};
    let names = names();
    names::path(names, names.root(krate), Namespace::Values, Written { kind: PathKind::Crate, segments }, Alloc::global())
        .unwrap_or_else(|e| panic!("`crate::{}` of crate {krate}: {e:?}", segments.join("::")))
        .def
        .key
}

/// `text` lowered with expansion in crate `krate`'s root module, then every path of the body
/// asked as the ladder's resolver asks it (`to_path` at the body's crate, then the names library
/// at the body's module): each path as written and the library item it names; and the dump.
fn asked_paths(krate: usize, text: &str) -> (StdVec<(String, Option<blaze_id::ItemKey>)>, String) {
    use crate::resolve::{Answer, Ns, resolve};
    use blaze_scope::names::{self, Written};
    use blaze_scope::task::to_path;
    use blaze_scope::Namespace;
    /// Each value path asked, as written, and the library item it names from the crate's root.
    struct Ask(&'static Names<'static>, usize, CrateItemKey, StdVec<(String, Option<blaze_id::ItemKey>)>);
    impl crate::resolve::Names for Ask {
        fn lookup(&mut self, ns: Ns, absolute: bool, segments: &[&str], mark: Option<crate::mark_site::TextMark>) -> Answer {
            let ns = match ns {
                Ns::Type => Namespace::Types,
                Ns::Value => Namespace::Values,
                Ns::Macro => Namespace::Macros,
            };
            let first = segments.first().copied().unwrap_or("");
            let hit = match crate::mark_site::site_of(mark, first, absolute, self.0.root(self.1)) {
                Ok(site) => {
                    // `$crate` names the mark's crate; `self.2` is read for no other path.
                    let mut p = to_path(absolute, segments, site.dollar_crate.unwrap_or(self.2), Alloc::global());
                    // token-mark-2: an item text writes `crate` for another crate's `$crate`; its
                    // mark's crate is `site.dollar_crate` (blaze-scope `crate_roots::root_by_mark`).
                    if let (Some(k), false, Some(&"crate")) = (site.dollar_crate, absolute, segments.first()) {
                        p.kind = blaze_scope::PathKind::DollarCrate(k);
                    }
                    match names::path(self.0, site.module, ns, Written { kind: p.kind, segments: &p.segments }, Alloc::global()) {
                        Ok(r) if r.consumed == p.segments.len() => Some(r.def.key),
                        _ => None,
                    }
                }
                Err(_) => None,
            };
            self.3.push((segments.join("::"), hit));
            Answer::Absent
        }
        fn lang(&mut self, _: crate::form::LangItem) -> Answer {
            Answer::Absent
        }
    }
    let names = names();
    let scoped = Scoped { names, module: names.root(krate), prepared: Some(Prepares::new_in(test_bump())) };
    let mut expander = ByName::with_names(&scoped, Host, Alloc::global());
    let l = lower_body_expanding(text, names.edition(krate), &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()).unwrap_or_else(|need| panic!("{text}: needs {:?}", need.atoms));
    let name = ["user", "dep", "old"][krate];
    let key = CrateItemKey(ContentId::of("blaze-lower scoped test crate", name.as_bytes()));
    let mut ask = Ask(names, krate, key, StdVec::new());
    resolve(&l.view(), &mut ask, Alloc::global()).unwrap_or_else(|e| panic!("{text}: {e:?}"));
    (ask.3, std::format!("{text}\n{}", dump_of(&l)))
}

/// The same rule for a macro handed through an `item` fragment of another macro (tokio's
/// `doc!` wrapper over `try_join!`), whose `$crate::try_join!` calls itself again.
#[test]
fn dollar_crate_in_a_macro_passed_through_an_item_fragment() {
    assert_clean("fn j() -> u32 { dep::try_join!(5) }", "5");
}

/// The Reference, "Macros By Example": a `path` fragment may name the macro of a call
/// (`$assert!(..)`), and Hygiene: `$crate` "refers to the crate defining the macro" that wrote
/// it, through any macro it is handed to. `dep::sometimes!` hands `$crate::m` to `guided!` (the
/// shape of antithesis_sdk's `assert_sometimes_greater_than!`, tempo's `fanout`); `user` has no
/// `m`, so only `dep`'s can expand it. Before the fix the fragment's invisible group hid the
/// path's first word and its mark, and the call was refused as "`$crate` outside an
/// expansion". Expected: `test scoped_tests::dollar_crate_in_a_path_fragment_names_the_writing_crate ... ok`.
#[test]
fn dollar_crate_in_a_path_fragment_names_the_writing_crate() {
    assert_clean("fn x() -> u32 { dep::sometimes!(44) }", "44");
    // A path the CALLER hands in carries no mark: it is read at the call, as written there.
    assert_clean("fn y() -> u32 { dep::guided!(dep::m, 45) }", "45");
}

/// The negative twin: a path fragment naming a macro the caller's module does not have is
/// still refused, a finding at the call naming the path (`m` is `dep`'s, never `user`'s, and a
/// fragment written in `user`'s text is read in `user`).
#[test]
fn a_path_fragment_naming_no_macro_is_a_finding() {
    let text = "fn z() -> u32 { dep::guided!(m, 46) }";
    let (dump, unhandled, findings) = lowered(text);
    assert!(unhandled > 0, "{text}:\n{dump}");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].0.replace(' ', ""), "m", "{findings:?}");
    assert!(!findings[0].2.contains("$crate"), "a plain path is not a `$crate` refusal: {findings:?}");
}

/// The standard library's `option_env!` (core macros/mod.rs): "If the named environment
/// variable is present at compile time, this will expand into an expression of type
/// `Option<&'static str>` whose value is `Some` of the value of the environment variable. If
/// the environment variable is not present, then this will expand to `None`." The answer is
/// an outside read: it joins the body's read list ([`ByName::reads`]), present or absent, so
/// setting the variable later reruns this body.
#[test]
fn an_environment_read_joins_the_body_s_read_list() {
    let names = names();
    let scoped = Scoped { names, module: names.root(0), prepared: Some(Prepares::new_in(test_bump())) };
    let mut expander = ByName::with_names(&scoped, Host, Alloc::global());
    let text = "fn r() { let _ = dep::option_env!(\"ABSENT\"); }";
    let l = lower_body_expanding(text, names.edition(0), &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()).unwrap_or_else(|need| panic!("{text}: needs {:?}", need.atoms));
    assert_eq!(l.view().unhandled().count(), 0, "{}", dump_of(&l));
    let reads = expander.reads();
    let seen: StdVec<(blaze_expand::ReadKind, &str)> = reads.list().iter().map(|r| (r.kind, reads.subject(r))).collect();
    assert_eq!(seen, [(blaze_expand::ReadKind::Env, "ABSENT")]);
}

/// A call whose reads cannot join the body's read list is not taken (its expansion would be
/// kept without what it read), and is never dropped quietly: it is a finding naming the call
/// and the bound. With a cap of one read, the second `option_env!` (the Reference's
/// `option_env!`, as above) passes it.
#[test]
fn a_call_past_the_read_list_s_cap_is_a_finding_naming_the_cap() {
    let names = names();
    let scoped = Scoped { names, module: names.root(0), prepared: Some(Prepares::new_in(test_bump())) };
    let mut expander = ByName::with_names(&scoped, Host, Alloc::global()).with_read_cap(1);
    let text = "fn r() { let _ = dep::option_env!(\"A\"); let _ = dep::option_env!(\"B\"); }";
    let l = lower_body_expanding(text, names.edition(0), &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()).unwrap_or_else(|need| panic!("{text}: needs {:?}", need.atoms));
    assert_eq!(l.view().unhandled().count(), 1, "{}", dump_of(&l));
    let reads = expander.reads();
    let seen: StdVec<&str> = reads.list().iter().map(|r| reads.subject(r)).collect();
    assert_eq!(seen, ["A"]);
    let findings: StdVec<(String, u32, String)> =
        expander.findings().iter().map(|f| (String::from(&*f.path), f.at.start, std::format!("{}", f.why))).collect();
    let second = u32::try_from(text.rfind("dep::option_env").unwrap()).unwrap();
    assert_eq!(findings, [(String::from("dep::option_env"), second, String::from("its reads would pass the body's read list's cap of 1"))]);
}

/// A host whose calling file is `src/lib.rs` of the unit `root` (its origin and its package's
/// folder under the root: the value a driver gives per unit, D60), with `files` named under
/// the root: the include reads the way blaze-real's `BodyHost` does
/// (`blaze_expand::include_target`, then the root's files). `OUT_DIR` paths are
/// blaze-build-script's `ScriptCx`'s (its test `a_path_the_script_printed_into_out_dir_names_the_records_file`).
struct FileHost {
    root: blaze_expand::ReadRoot<'static>,
    files: &'static [(&'static str, &'static [u8])],
}

/// A member at `a` of a checkout, with three files of its own.
const FILE_HOST: FileHost = FileHost {
    root: blaze_expand::ReadRoot { origin: blaze_expand::SourceOrigin::Checkout, package: "a" },
    files: &[("a/src/data.txt", b"hi"), ("a/src/more.rs", b"40 + 2"), ("a/bin.dat", b"\x00\xff")],
};

impl blaze_expand::ExpandCx for FileHost {
    fn env(&self, _: &str) -> Result<Option<&str>, blaze_expand::HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &blaze_expand::TokenStream<'_>) -> Result<bool, blaze_expand::HostError> {
        Ok(false)
    }
    fn include(&self, written: &str, _: blaze_expand::IncludeAs) -> Result<blaze_expand::Included<'_>, blaze_expand::HostError> {
        let Ok(target) = blaze_expand::include_target(self.root, "src", written, Alloc::global()) else {
            return Ok(blaze_expand::Included::Outside);
        };
        let path = target.join("/");
        Ok(match self.files.iter().find(|(p, _)| *p == path) {
            Some((_, bytes)) => blaze_expand::Included::Bytes(bytes),
            None => blaze_expand::Included::Missing,
        })
    }
    fn place(&self, what: blaze_expand::PlaceOf) -> Result<blaze_expand::Place<'_>, blaze_expand::HostError> {
        blaze_expand::ExpandCx::place(&Host, what)
    }
    fn run_proc_macro<'a>(
        &self,
        def: &blaze_expand::MacroDefinition<'_>,
        call: blaze_expand::ProcCall<'_>,
        alloc: Alloc<'a>,
    ) -> Result<blaze_expand::Produced<'a>, blaze_expand::HostError> {
        blaze_expand::ExpandCx::run_proc_macro(&Host, def, call, alloc)
    }
}

/// `text` lowered over [`FileHost`]: the unhandled count and the body's reads.
fn included(text: &str) -> (usize, StdVec<(blaze_expand::ReadKind, String)>) {
    included_over(text, FILE_HOST)
}

/// [`included`] over the host `cx`.
fn included_over<C: blaze_expand::ExpandCx>(text: &str, cx: C) -> (usize, StdVec<(blaze_expand::ReadKind, String)>) {
    let names = names();
    let scoped = Scoped { names, module: names.root(0), prepared: Some(Prepares::new_in(test_bump())) };
    let mut expander = ByName::with_names(&scoped, cx, Alloc::global());
    let l = lower_body_expanding(text, names.edition(0), &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()).unwrap_or_else(|need| panic!("{text}: needs {:?}", need.atoms));
    let reads = expander.reads();
    (l.view().unhandled().count(), reads.list().iter().map(|r| (r.kind, String::from(reads.subject(r)))).collect())
}

/// The expected [`included`] answer.
fn want(unhandled: usize, reads: &[(blaze_expand::ReadKind, &str)]) -> (usize, StdVec<(blaze_expand::ReadKind, String)>) {
    (unhandled, reads.iter().map(|(k, s)| (*k, String::from(*s))).collect())
}

/// The pinned library's `include_str!`, `include_bytes!` and `include!` (core macros/mod.rs):
/// "The file is located relative to the current file (similarly to how modules are found)";
/// each read is tracked (the written path and the content's digest join the body's read
/// list), a missing file is the call's error, and a path that leaves the unit's source root
/// (D60) is refused by its path (`IncludeFailure::Outside`), never read and never a read.
#[test]
fn includes_read_beside_the_calling_file_and_never_leave_the_source_root() {
    use blaze_expand::ReadKind::{IncludeBytes, IncludeStr};
    assert_eq!(included("fn s() -> &'static str { dep::include_str!(\"data.txt\") }"), want(0, &[(IncludeStr, "data.txt")]));
    assert_eq!(included("fn b() -> &'static [u8; 2] { dep::include_bytes!(\"../bin.dat\") }"), want(0, &[(IncludeBytes, "../bin.dat")]));
    // `include!` reads the file as text (the same fact as `include_str!`) and parses it here.
    assert_eq!(included("fn i() -> u32 { dep::include!(\"./more.rs\") }"), want(0, &[(IncludeStr, "./more.rs")]));
    // A missing file: an error of the call, its absence a read (creating it reruns the body).
    assert_eq!(included("fn m() -> &'static str { dep::include_str!(\"none.txt\") }"), want(1, &[(IncludeStr, "none.txt")]));
    // Past the source root: refused by its path, no read.
    assert_eq!(included("fn o() -> &'static str { dep::include_str!(\"../../../b/data.txt\") }"), want(1, &[]));
    assert_eq!(included("fn p() -> &'static str { dep::include_str!(\"/etc/passwd\") }"), want(1, &[]));
}

/// Decision D60 (docs/v2/plan/decisions.json): "A workspace member may read any file under the
/// workspace's source root ... A fetched unit may read under its fetched root: the whole
/// checkout for a git source, the package for a registry one", and "A path that leaves the
/// bound is a typed refusal naming the call, the path and this decision". tempo's shape:
/// `crates/node` reads `../../crates/chainspec/src/genesis/moderato.json`.
#[test]
fn an_include_reads_under_the_unit_s_source_root_and_is_refused_past_it() {
    use blaze_expand::ReadKind::IncludeStr;
    use blaze_expand::SourceOrigin::{Checkout, GitCheckout, Package};
    use blaze_expand::ReadRoot;
    let call = |path: &str| std::format!("fn g() -> &'static str {{ dep::include_str!(\"{path}\") }}");
    let member = FileHost { root: ReadRoot { origin: Checkout, package: "crates/node" }, files: &[("crates/chainspec/g.json", b"{}")] };
    assert_eq!(included_over(&call("../../chainspec/g.json"), member), want(0, &[(IncludeStr, "../../chainspec/g.json")]));
    let git = FileHost { root: ReadRoot { origin: GitCheckout, package: "pkg" }, files: &[("shared/x.txt", b"x")] };
    assert_eq!(included_over(&call("../../shared/x.txt"), git), want(0, &[(IncludeStr, "../../shared/x.txt")]));
    let registry = FileHost { root: ReadRoot { origin: Package, package: "reg-1.0.0" }, files: &[("shared/x.txt", b"x")] };
    assert_eq!(included_over(&call("../../shared/x.txt"), registry), want(1, &[]));
    let past = FileHost { root: ReadRoot { origin: Checkout, package: "crates/node" }, files: &[] };
    assert_eq!(included_over(&call("../../../../etc/x"), past), want(1, &[]));
    // The refusal names the call's path and the decision.
    let names = names();
    let scoped = Scoped { names, module: names.root(0), prepared: Some(Prepares::new_in(test_bump())) };
    let past = FileHost { root: ReadRoot { origin: Package, package: "reg" }, files: &[] };
    let mut expander = ByName::with_names(&scoped, past, Alloc::global());
    let text = call("../../x");
    lower_body_expanding(&text, names.edition(0), &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()).unwrap_or_else(|need| panic!("needs {:?}", need.atoms));
    let why: StdVec<String> = expander.findings().iter().map(|f| std::format!("{}", f.why)).collect();
    assert_eq!(why, [String::from("its expansion's error Include (the path leaves the unit's source root: decision D60): `../../x`")]);
}

/// D60: "the content's digest in the key of exactly the body or item that holds the call, so
/// an edit of the file rebuilds only what includes it". A body's record is valid by its read
/// list: an edit of the included file changes the digest in the including body's list, and
/// the list of a body beside it that includes nothing stays equal.
#[test]
fn an_edit_of_an_included_file_changes_only_the_including_body_s_reads() {
    use blaze_expand::ReadRoot;
    use blaze_expand::SourceOrigin::Checkout;
    let reads = |text: &str, bytes: &'static [u8]| {
        let names = names();
        let scoped = Scoped { names, module: names.root(0), prepared: Some(Prepares::new_in(test_bump())) };
        let host = FileHost { root: ReadRoot { origin: Checkout, package: "a" }, files: std::vec![("b/g.json", bytes)].leak() };
        let mut expander = ByName::with_names(&scoped, host, Alloc::global());
        lower_body_expanding(text, names.edition(0), &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()).unwrap_or_else(|need| panic!("needs {:?}", need.atoms));
        let mut bytes = blaze_own::Vec::new_in(Alloc::global());
        expander.reads().encode(&mut bytes);
        StdVec::from(&bytes[..])
    };
    let includer = "fn g() -> &'static str { dep::include_str!(\"../../b/g.json\") }";
    let beside = "fn h() -> u32 { 1 }";
    assert_ne!(reads(includer, b"{\"v\":1}"), reads(includer, b"{\"v\":2}"));
    assert_eq!(reads(beside, b"{\"v\":1}"), reads(beside, b"{\"v\":2}"));
}

/// The pinned library's `include!` (core macros/mod.rs) takes an expression its expander
/// expands first: `include!(concat!(env!("OUT_DIR"), "/x.rs"))` is the generated-code form
/// (its doc: "using the `concat!` and `env!` macros"). `OUT_DIR` is the build script's: the
/// file comes from the script record's generated files (blaze-build-script's `ScriptCx`),
/// never from the host; the body reads `OUT_DIR` and the file's path. Without the record's
/// mapping the host has no `OUT_DIR`, and the call fails.
#[test]
fn an_include_of_out_dir_reads_the_script_record_s_file() {
    use blaze_build_script::outputs::{FileLimits, Outputs, OutputsRef};
    use blaze_build_script::instr::OutputLimits;
    use blaze_expand::ReadKind::{Env, IncludeStr};
    const OUT: &str = "/target/debug/build/a-0123456789abcdef/out";
    let alloc = Alloc::global();
    let mut out = Outputs::read(b"", OUT, OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    out.add_file("x.rs", b"40 + 2", FileLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    let mut bytes = blaze_own::Vec::new_in(alloc);
    out.encode(&mut bytes);
    let record = OutputsRef::open(&bytes).unwrap_or_else(|e| panic!("{e:?}"));
    let text = "fn i() -> u32 { dep::include!(dep::concat!(dep::env!(\"OUT_DIR\"), \"/x.rs\")) }";
    let path = std::format!("{OUT}/x.rs");
    assert_eq!(
        included_over(text, blaze_build_script::cx::ScriptCx { inner: FILE_HOST, out: record }),
        want(0, &[(Env, "OUT_DIR"), (IncludeStr, &path)])
    );
    // The host alone: no `OUT_DIR`, the call's error and its read.
    assert_eq!(included(text), want(1, &[(Env, "OUT_DIR")]));
}

/// The Reference, "Macros By Example", Scoping: a `macro_rules!` is in textual scope after
/// it is written, in nested blocks too, until the end of the block it is written in; textual
/// scope is tried before path-based scope. `user` reaches `dep`'s `seven_of` by `use`.
#[test]
fn textual_scope_shadows_path_based_scope() {
    const LOCAL: &str = "macro_rules! seven_of { () => { 4242 } }";
    let local_wins = |text: &str, wins: bool| {
        let (dump, unhandled, findings) = lowered(text);
        assert_eq!((unhandled, findings.len()), (0, 0), "{text}: {findings:?}\n{dump}");
        assert_eq!((dump.contains("4242"), dump.contains("SEVEN")), (wins, !wins), "{text}:\n{dump}");
    };
    // After its statement: the body's own.
    local_wins(&std::format!("fn f() -> u32 {{ {LOCAL} seven_of!() }}"), true);
    // Before it: the path-based one.
    local_wins(&std::format!("fn f() -> u32 {{ let a = seven_of!(); {LOCAL} a }}"), false);
    // In a nested block after it: the body's own.
    local_wins(&std::format!("fn f() -> u32 {{ {LOCAL} {{ seven_of!() }} }}"), true);
    // After the block it is written in ends: the path-based one.
    local_wins(&std::format!("fn f() -> u32 {{ {{ {LOCAL} }} seven_of!() }}"), false);
    // The last written of a name wins.
    local_wins(&std::format!("fn f() -> u32 {{ macro_rules! seven_of {{ () => {{ 1 }} }} {LOCAL} seven_of!() }}"), true);
}

#[test]
fn an_unresolved_macro_is_a_finding_at_the_call() {
    let text = "fn g() { let x = 1; dep::nothing!(); }";
    let (_, unhandled, findings) = lowered(text);
    assert_eq!(unhandled, 1);
    let start = u32::try_from(text.find("dep::nothing").expect("written")).expect("small");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!((findings[0].0.as_str(), findings[0].1), ("dep::nothing", start), "{findings:?}");
    assert!(findings[0].2.contains("segment 1"), "the path's second segment names nothing: {findings:?}");
    // The typed cause, not only its text.
    let names = names();
    let scoped = Scoped { names, module: names.root(0), prepared: None };
    let why = crate::by_name::MacroNames::resolve(&scoped, "dep::nothing", Default::default(), blaze_expand::ProcKind::FnLike, Alloc::global())
        .map(|_| ());
    assert!(matches!(why, Err(NoMacro::Names(MacroDefError::Path(_)))), "{why:?}");
}

/// The library's `panic!` (`#[rustc_builtin_macro(core_panic)]`, and `std_panic` in std) is a
/// builtin that "expands to either `$crate::panic::panic_2015` or `$crate::panic::panic_2021`
/// depending on the edition of the caller" (core's macros/mod.rs; the Edition Guide, Rust 2021,
/// "Panic macro consistency": in 2021 `panic!` always takes a format string, in 2015 a single
/// argument is the payload as written, braces and all). The caller's edition is its crate's.
#[test]
fn panic_picks_its_form_by_the_call_site_s_edition() {
    const OLD: usize = 2;
    // `panic!("{}", x)`: the special-cased display form in both editions.
    for krate in [0, OLD] {
        let (dump, unhandled, findings) = lowered_in(krate, "fn a(x: u32) { dep::panic!(\"{}\", x) }");
        assert_eq!((unhandled, findings.len()), (0, 0), "crate {krate}: {findings:?}\n{dump}");
        assert!(dump.contains("panic_display"), "crate {krate}:\n{dump}");
    }
    // `panic!(x)` in 2015: `x` is the payload, no format string.
    let (dump, unhandled, findings) = lowered_in(OLD, "fn b(x: &str) { dep::panic!(x) }");
    assert_eq!((unhandled, findings.len()), (0, 0), "{findings:?}\n{dump}");
    assert!(dump.contains("panic_str_2015"), "{dump}");
    // In 2021 the same call is a format string that is not a literal: refused.
    let (dump, unhandled, _) = lowered_in(0, "fn b(x: &str) { dep::panic!(x) }");
    assert!(unhandled > 0 && !dump.contains("panic_str_2015"), "{dump}");
    // A literal with braces: in 2015 the message as written; in 2021 a format string, so
    // `"{}"` with no argument is refused and `"{{}}"` formats.
    let (dump, unhandled, findings) = lowered_in(OLD, "fn c() { dep::panic!(\"{}\") }");
    assert_eq!((unhandled, findings.len()), (0, 0), "{findings:?}\n{dump}");
    assert!(!dump.contains("panic_fmt") && !dump.contains("diag"), "{dump}");
    let (dump, _, _) = lowered_in(0, "fn c() { dep::panic!(\"{}\") }");
    assert!(dump.contains("panic_fmt") && dump.contains("diag FormatArgMissing"), "a 2021 format string with a missing argument:\n{dump}");
    let (dump, unhandled, findings) = lowered_in(0, "fn d() { dep::panic!(\"{{}}\") }");
    assert_eq!((unhandled, findings.len()), (0, 0), "{findings:?}\n{dump}");
    assert!(dump.contains("panic_fmt") && !dump.contains("diag"), "{dump}");
}

/// The caller `panic!` picks its form by is the code that writes the call: core's macros/mod.rs
/// says `panic!` "expands to either `$crate::panic::panic_2015` or `$crate::panic::panic_2021`
/// depending on the edition of the caller", and a `panic!` inside a `macro_rules!` body is
/// text of the crate that defines the macro (the Reference, "Macros By Example", Hygiene:
/// tokens of the definition keep its context; the Edition Guide, Rust 2021, "Panic macro
/// consistency"). So `old` (2015)'s `fail!() => panic!("{}")` called from `user` (2021) is the
/// 2015 form: the message as written, no format string, no missing argument.
#[test]
fn a_panic_written_in_a_macro_takes_the_defining_crate_s_edition() {
    let (dump, unhandled, findings) = lowered_in(0, "fn e() { old::fail!() }");
    assert_eq!((unhandled, findings.len()), (0, 0), "{findings:?}\n{dump}");
    assert!(!dump.contains("panic_fmt") && !dump.contains("diag"), "the 2015 form, as `old` writes it:\n{dump}");
}

/// [`Scoped`], recording each path asked and the written call it is read at
/// (`PathAt::written`).
struct Recorded<'d, 'p: 'd> {
    inner: Scoped<'d, 'p>,
    seen: core::cell::RefCell<StdVec<(String, Option<blaze_surface::Range>)>>,
}

impl<'d, 'p: 'd> crate::by_name::MacroNames<'d> for Recorded<'d, 'p> {
    fn resolve<'a>(
        &self,
        path: &str,
        at: crate::by_name::PathAt,
        kind: blaze_expand::ProcKind,
        alloc: Alloc<'a>,
    ) -> Result<crate::by_name::Resolved<'d, 'a>, NoMacro<'a>>
    where
        'd: 'a,
    {
        self.seen.borrow_mut().push((path.replace(' ', ""), at.written));
        crate::by_name::MacroNames::resolve(&self.inner, path, at, kind, alloc)
    }

    fn local_site(&self) -> Option<blaze_expand::DefSite> {
        crate::by_name::MacroNames::local_site(&self.inner)
    }
}

/// `text` lowered over [`Recorded`]: each path asked, with where it is read.
fn read_at(text: &str) -> StdVec<(String, Option<blaze_surface::Range>)> {
    let names = names();
    let recorded = Recorded {
        inner: Scoped { names, module: names.root(0), prepared: Some(Prepares::new_in(test_bump())) },
        seen: core::cell::RefCell::new(StdVec::new()),
    };
    let mut expander = ByName::with_names(&recorded, Host, Alloc::global());
    lower_body_expanding(text, names.edition(0), &mut Unix, &mut crate::NoModule, &mut expander, Alloc::global()).unwrap_or_else(|need| panic!("{text}: needs {:?}", need.atoms));
    drop(expander);
    recorded.seen.into_inner()
}

/// The Reference, "Macros By Example", Hygiene: a path is looked up "as if it appeared at the
/// invocation site", and "Statements": an item a block declares is in scope in that block. So a
/// call an expansion wrote reads a call-site path where the WRITTEN call it came from is: tempo's
/// `fn t() { use serde_json::json; assert_eq!(x, json!("a")); }` (crates/primitives/tests/
/// configurable_account.rs) has `json!` in `assert_eq!`'s expansion, and `json` only in the
/// fn's block. Before the fix the inner call had no written range and was read in the body's
/// module ("segment 0: not found"). Each call here, the inner ones at any depth, is read at
/// the one written call's range.
#[test]
fn a_call_an_expansion_wrote_is_read_at_the_written_call() {
    let text = "fn w() -> u32 { let a = 1; { dep::m!(dep::m!(dep::m!(8))) } }";
    let written = "dep::m!(dep::m!(dep::m!(8)))";
    let start = text.find(written).expect("written");
    let call = Some(blaze_surface::Range::new(u32::try_from(start).expect("small"), u32::try_from(start + written.len()).expect("small")));
    assert_eq!(read_at(text), [(String::from("dep::m"), call), (String::from("dep::m"), call), (String::from("dep::m"), call)]);
}

/// The negative twin: two written calls, each with a call in its expansion: each inner call is
/// read at ITS written call, never at the other or at the body (a walk that stopped at the
/// first expansion, or took the last call, would give one range to both).
#[test]
fn calls_in_two_expansions_are_read_at_their_own_written_calls() {
    let text = "fn v() -> u32 { dep::m!(dep::m!(1)) + dep::m!(dep::m!(2)) }";
    let first = text.find("dep::m!(dep::m!(1))").expect("written");
    let second = text.find("dep::m!(dep::m!(2))").expect("written");
    let len = "dep::m!(dep::m!(1))".len();
    let range = |at: usize| Some(blaze_surface::Range::new(u32::try_from(at).expect("small"), u32::try_from(at + len).expect("small")));
    let seen = read_at(text);
    let ranges: StdVec<_> = seen.iter().map(|(_, r)| *r).collect();
    assert_eq!(ranges, [range(first), range(first), range(second), range(second)], "{seen:?}");
}
