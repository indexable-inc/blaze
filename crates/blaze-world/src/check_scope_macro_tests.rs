//! check_scope.rs over a workspace that uses a proc macro (2026-11-17: three small projects
//! deriving `clap::Parser`, `serde::Serialize` and `thiserror::Error` each refused every
//! `ImplBucket` task of their derive crate with "the bodies of `clap_derive` are checked on the
//! Macro side, and the edge `clap_derive` -> `proc_macro` reaches `proc_macro` of the other
//! side"). The cause: a DEPENDENCY's proc macro is kept by the list for its macros only, on the
//! target side, with no dependency but the target toolchain's `proc_macro` (blaze-scope
//! units.rs `unit_crates`, list.rs `world`), and `check_side` took every proc macro for a
//! macro-side owner. The rule now: such a crate has no check scope (`ScopeError::MacrosOnly`,
//! `has_check_scope`), and every other crate's scope can be made, its crates all of its side
//! or seen for their macros only. A child of list_tests.rs, over its fixture's helpers (its
//! allocator `a()` among them).

use core::fmt::Write as _;

use blaze_own::{AString, AVec};

use super::*;
use crate::check_scope::{ScopeError, Seen, check_scope_in, has_check_scope};
use crate::crate_list::{CrateGraph, CrateList, encode};

/// `app` (member) depends on `derive` (a proc macro, `proc_macro::TokenStream`'s user) and on
/// `toks` (registry), which `derive` uses too: `syn`'s shape. `derive`'s place is the test's.
const LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["derive", "toks"]

[[package]]
name = "derive"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
dependencies = ["toks"]

[[package]]
name = "toks"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

/// [`LOCK`] with `derive` a workspace member (a path package in the checkout).
const MEMBER_LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["derive", "toks"]

[[package]]
name = "derive"
version = "0.1.0"
dependencies = ["toks"]

[[package]]
name = "toks"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

/// `value` for the rest of the test, held in the fixture's allocator.
fn held<T: 'static>(value: T) -> &'static T {
    let mut one: AVec<'static, T> = AVec::with_capacity_in(1, a());
    one.push(value);
    let kept: &'static [T] = one.leak_static();
    &kept[0]
}

/// The members in the checkout at their own directories; every other package the one pack.
struct Placed {
    lock: &'static Lock<'static>,
    members: &'static [&'static str],
    pack: &'static [u8],
}

impl Packages<'static> for Placed {
    fn place(&mut self, package: LockId) -> Result<Place<'static>, WorldError<'static>> {
        let name = self.lock.package(package).name.as_str();
        match self.members.iter().find(|m| **m == name) {
            Some(dir) => Ok(Place::Checkout { dir: *dir }),
            None => Ok(Place::Pack { view: PackView::open_verified(self.pack).expect("a pack"), dir: "" }),
        }
    }
}

/// The world of the workspace: 0 `toks` for the target, 1 `toks` for the host, 2 `derive`
/// (proc macro, on the host's `toks`), 3 `app` (on the target's `toks` and on `derive`);
/// `derive` a member when `member`, over the fixture's shelf cut for the macro side or not.
fn derive_world(member: bool, macro_side: bool) -> World<'static> {
    let text = if member { MEMBER_LOCK } else { LOCK };
    let lock: &'static Lock<'static> = held(Lock::read(text.as_bytes(), a()).expect("the lock reads"));
    let mut units = AVec::new_in(a());
    units.push(unit(lock, "toks", UnitKind::Lib, false, &[]));
    units.push(unit(lock, "toks", UnitKind::Lib, true, &[]));
    units.push(unit(lock, "derive", UnitKind::ProcMacro, true, &[("toks", 1)]));
    units.push(unit(lock, "app", UnitKind::Lib, false, &[("toks", 0), ("derive", 2)]));
    let graph: &'static UnitGraph<'static> = held(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) });
    let made = crate::fixture::pack(&[("src/lib.rs", "pub struct TokenStream;\npub struct Expr;\n")], a());
    let mut bytes: AVec<'static, u8> = AVec::with_capacity_in(made.len(), a());
    bytes.extend_from_slice(&made);
    let pack: &'static [u8] = bytes.leak_static();
    let members: &'static [&'static str] = if member { &["app", "derive"] } else { &["app"] };
    let mut places = Placed { lock, members, pack };
    let (id, mut shelf) = shelf(macro_side);
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut places, b()).unwrap_or_else(|e| panic!("{e}"));
    let listed = held(listed);
    world(graph, lock, listed, &mut places, &NoFiles, b()).unwrap_or_else(|e| panic!("{e}"))
}

/// The crate named `name` on `side`, if the world holds one.
fn named(w: &World<'static>, name: &str, side: Side) -> Option<usize> {
    (0..w.crates.len()).find(|c| w.crates[*c].input.name == name && w.crates[*c].side == side)
}

/// Every crate of `w` with a check scope has one (over the world and over its crate list, the
/// two alike), its owner first, each crate it links of its check side, each other crate a proc
/// macro seen for its macros; a crate without one is refused by name with the reason
/// [`has_check_scope`] gave. The owners scoped, and those without a scope, by name.
fn every_scope(w: &World<'static>, what: &str) -> (usize, AVec<'static, &'static str>) {
    let bytes = encode(w, &|_| None, a()).unwrap_or_else(|e| panic!("{what}: {e}"));
    let bytes: &'static [u8] = bytes.leak_static();
    let list = CrateList::open(bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
    let mut scoped = 0;
    let mut without: AVec<'static, &'static str> = AVec::new_in(a());
    for owner in 0..w.crates.len() {
        let name = w.crates[owner].input.name;
        let row = list.find(w.crates[owner].input.key).expect("every crate has its row");
        assert_eq!(has_check_scope(w, owner), has_check_scope(&list, row), "{what}: `{name}`: the world and its list say one thing");
        if !has_check_scope(w, owner) {
            let refused = check_scope_in(w, owner, false, a()).err().unwrap_or_else(|| panic!("{what}: `{name}` has no scope, yet one was made"));
            assert!(matches!(refused, ScopeError::MacrosOnly { krate } | ScopeError::Refused { krate } if krate == name), "{what}: `{name}` is refused by its own name: {refused}");
            without.push(name);
            continue;
        }
        let scope = check_scope_in(w, owner, false, a()).unwrap_or_else(|e| panic!("{what}: the scope of `{name}`: {e}"));
        let side = scope.side();
        assert_eq!(scope.readers()[0], (owner, Seen::Owner), "{what}: `{name}` is first in its scope");
        for &(at, how) in scope.readers() {
            let seen = &w.crates[at];
            match how {
                Seen::Owner => assert_eq!(at, owner, "{what}: one owner"),
                Seen::Linked { .. } => assert_eq!(seen.side, side, "{what}: `{name}` on {side:?} links `{}` of {:?}", seen.input.name, seen.side),
                Seen::MacrosOnly => assert!(seen.proc_macro, "{what}: `{name}` sees `{}` for its macros, which is no proc macro", seen.input.name),
            }
        }
        let by_list = check_scope_in(&list, row, false, a()).unwrap_or_else(|e| panic!("{what}: the scope of `{name}` over the list: {e}"));
        let mut world_keys: AVec<'static, ([u8; 32], Seen)> = AVec::new_in(a());
        world_keys.extend(scope.readers().iter().map(|(at, how)| (w.crates[*at].input.key.0.0, *how)));
        world_keys.sort_unstable_by_key(|(k, _)| *k);
        let mut list_keys: AVec<'static, ([u8; 32], Seen)> = AVec::new_in(a());
        list_keys.extend(by_list.readers().iter().map(|(r, how)| (list.key(*r).0.0, *how)));
        list_keys.sort_unstable_by_key(|(k, _)| *k);
        assert_eq!(&world_keys[..], &list_keys[..], "{what}: `{name}` sees other crates over the list");
        assert_eq!(by_list.side(), side, "{what}: `{name}`'s side over the list");
        scoped += 1;
    }
    assert!(CrateGraph::len(&list) == w.crates.len(), "{what}: a row a crate");
    (scoped, without)
}

/// (a) A REGISTRY PROC MACRO: every crate of the world but `derive` has its scope (`app`, `toks`,
/// each toolchain crate), on the target side; `derive`, kept for its macros only with the target
/// toolchain's `proc_macro` as its one dependency, has none, refused as `MacrosOnly` naming it,
/// and `app` sees it for its macros. The same on a shelf cut for the macro side (no member proc
/// macro, so no macro side is made). Before the fix `derive`'s scope was refused as `OtherSide`
/// `derive` -> `proc_macro` (the real note; see [`a_registry_proc_macro_asked_as_macro_side_is_the_real_refusal`]).
/// Prints `macro scope (a): <n> scopes, none for [derive]`.
#[test]
fn a_registry_proc_macro_has_no_scope_and_every_other_crate_has_one() {
    for macro_side in [false, true] {
        let w = derive_world(false, macro_side);
        assert!(w.refused.is_empty(), "nothing is refused: {:?}", w.refused);
        assert!(w.crates.iter().all(|c| c.side == Side::Target), "no member proc macro, no macro side");
        let derive = w.crate_of_unit[2].expect("derive is a crate") as usize;
        let app = w.crate_of_unit[3].expect("app is a crate") as usize;
        let pm = named(&w, "proc_macro", Side::Target).expect("the target toolchain's proc_macro");
        assert!(w.crates[derive].proc_macro, "derive is a proc macro");
        assert_eq!(w.crates[derive].input.deps, &[("proc_macro", pm)][..], "the list keeps a dependency's proc macro with the target's `proc_macro` alone (list.rs `world`)");
        assert!(!has_check_scope(&w, derive), "a dependency's proc macro has no check scope");
        assert_eq!(check_scope_in(&w, derive, false, a()).err(), Some(ScopeError::MacrosOnly { krate: "derive" }));
        let (scoped, without) = every_scope(&w, "registry derive");
        assert_eq!(&without[..], &["derive"][..], "derive alone has no scope");
        assert_eq!(scoped, w.crates.len() - 1);
        let app_scope = check_scope_in(&w, app, false, a()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(app_scope.side(), Side::Target);
        assert!(app_scope.readers().contains(&(derive, Seen::MacrosOnly)), "app names derive for its macros: {:?}", app_scope.readers());
        std::println!("macro scope (a), macro side cut {macro_side}: {scoped} scopes, none for {:?}", &without[..]);
    }
}

/// (b, c) A MEMBER PROC MACRO, the shelf cut for the macro side: every crate has its scope, each
/// toolchain crate of both sides among them; `derive` is checked on the macro side, its
/// `proc_macro` the macro side's; `toks` is two rows under two keys, one a side, and each scope
/// takes its own side's (`app` the target's, `derive` the macro side's). Without the cut,
/// `derive` is refused by the list (`NoMacroToolchain`) and has no scope, refused as `Refused`;
/// every other crate has one. Prints `macro scope (b): <n> scopes; toks rows <t>, <m>`.
#[test]
fn a_member_proc_macro_and_every_other_crate_have_their_scope() {
    let w = derive_world(true, true);
    assert!(w.refused.is_empty(), "with the cut nothing is refused: {:?}", w.refused);
    let derive = w.crate_of_unit[2].expect("derive is a crate") as usize;
    let app = w.crate_of_unit[3].expect("app is a crate") as usize;
    assert_eq!(w.crates[derive].side, Side::Macro, "a member proc macro is built on the macro side");
    let (scoped, without) = every_scope(&w, "member derive");
    assert!(without.is_empty(), "every crate has its scope: none for {:?}", &without[..]);
    assert_eq!(scoped, w.crates.len());
    assert!(named(&w, "core", Side::Macro).is_some() && named(&w, "proc_macro", Side::Macro).is_some(), "the macro side's toolchain crates are rows, and each was scoped above");
    // (c) `toks`, built for both sides: two rows, two keys, each scope its own.
    let target_toks = named(&w, "toks", Side::Target).expect("the target's toks");
    let side_toks = named(&w, "toks", Side::Macro).expect("the macro side's toks");
    assert_ne!(target_toks, side_toks, "two rows");
    assert_ne!(w.crates[target_toks].input.key.0.0, w.crates[side_toks].input.key.0.0, "two keys");
    let derive_scope = check_scope_in(&w, derive, false, a()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(derive_scope.side(), Side::Macro);
    let side_pm = w.crates[derive].input.deps.iter().find(|(n, _)| *n == "proc_macro").map(|(_, d)| *d).expect("derive names `proc_macro`");
    assert_eq!(w.crates[side_pm].side, Side::Macro, "derive's `proc_macro` is the macro side's");
    assert!(derive_scope.crates().any(|at| at == side_toks) && !derive_scope.crates().any(|at| at == target_toks), "derive takes the macro side's toks alone: {:?}", derive_scope.readers());
    let app_scope = check_scope_in(&w, app, false, a()).unwrap_or_else(|e| panic!("{e}"));
    assert!(app_scope.crates().any(|at| at == target_toks) && !app_scope.crates().any(|at| at == side_toks), "app takes the target's toks alone: {:?}", app_scope.readers());
    assert!(app_scope.readers().contains(&(derive, Seen::MacrosOnly)), "app names derive for its macros");
    std::println!("macro scope (b): {scoped} scopes; toks rows {target_toks}, {side_toks}");

    // Without the cut: derive refused by the list, and so without a scope; the rest scoped.
    let w = derive_world(true, false);
    let derive = w.crate_of_unit[2].expect("derive is still a crate") as usize;
    assert!(w.refusal(derive).is_some(), "derive is refused for want of the macro side");
    assert!(!has_check_scope(&w, derive));
    assert_eq!(check_scope_in(&w, derive, false, a()).err(), Some(ScopeError::Refused { krate: "derive" }));
    let (scoped, without) = every_scope(&w, "member derive, no cut");
    assert_eq!(&without[..], &["derive"][..]);
    std::println!("macro scope (b), no cut: {scoped} scopes, none for {:?}", &without[..]);
}

/// (d) RULE 23, `OtherSide` STAYS A REFUSAL OF A BROKEN LIST: the member `derive`'s
/// `proc_macro` edge planted on the target toolchain's row is refused naming the owner, the edge
/// and the side. CONTROL: the world as made gives `derive` its scope. Prints
/// `macro scope (d): <the refusal's words>`.
#[test]
fn a_member_proc_macro_given_the_target_proc_macro_is_refused() {
    let mut w = derive_world(true, true);
    let derive = w.crate_of_unit[2].expect("derive is a crate") as usize;
    assert!(check_scope_in(&w, derive, false, a()).is_ok(), "CONTROL: derive's scope as made");
    let target_pm = named(&w, "proc_macro", Side::Target).expect("the target toolchain's proc_macro");
    let old = w.crates[derive].input.deps;
    let mut deps: AVec<'static, (&'static str, usize)> = AVec::with_capacity_in(old.len(), a());
    deps.extend(old.iter().map(|&(n, d)| if n == "proc_macro" { (n, target_pm) } else { (n, d) }));
    w.crates[derive].input.deps = deps.leak_static();
    let refused = check_scope_in(&w, derive, false, a()).err().expect("an edge to the other side's proc_macro is refused");
    assert_eq!(refused, ScopeError::OtherSide { owner: "derive", from: "derive", krate: "proc_macro", side: Side::Macro });
    let mut words = AString::new_in(a());
    write!(words, "{refused}").expect("words");
    assert!(words.contains("`derive` -> `proc_macro`") && words.contains("Macro side"), "{}", words.as_str());
    std::println!("macro scope (d): {}", words.as_str());
}

/// (d) RULE 23, THE CAUSE: the registry `derive` asked for a macro-side scope, as `check_side`
/// took every proc macro before (its row planted on the macro side, which is what the old rule
/// assumed of it), meets the target toolchain's `proc_macro` the list gave it and is refused with
/// the real run's words ("the bodies of `derive` are checked on the Macro side, and the edge
/// `derive` -> `proc_macro` reaches `proc_macro` of the other side"). Unplanted, it has no scope
/// and asks for none (`MacrosOnly`). Prints `macro scope (cause): <the refusal's words>`.
#[test]
fn a_registry_proc_macro_asked_as_macro_side_is_the_real_refusal() {
    let mut w = derive_world(false, false);
    let derive = w.crate_of_unit[2].expect("derive is a crate") as usize;
    assert_eq!(check_scope_in(&w, derive, false, a()).err(), Some(ScopeError::MacrosOnly { krate: "derive" }), "CONTROL: as made, no scope, by name");
    w.crates[derive].side = Side::Macro;
    assert!(has_check_scope(&w, derive), "a macro-side proc macro has a scope to make");
    let refused = check_scope_in(&w, derive, false, a()).err().expect("the target's proc_macro is of the other side");
    assert_eq!(refused, ScopeError::OtherSide { owner: "derive", from: "derive", krate: "proc_macro", side: Side::Macro });
    let mut words = AString::new_in(a());
    write!(words, "{refused}").expect("words");
    assert!(words.contains("the bodies of `derive` are checked on the Macro side, and the edge `derive` -> `proc_macro` reaches `proc_macro` of the other side"), "{}", words.as_str());
    std::println!("macro scope (cause): {}", words.as_str());
}
