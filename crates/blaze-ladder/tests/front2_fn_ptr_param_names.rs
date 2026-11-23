//! A fn pointer type's parameter names bind nothing in the body that writes the type:
//! `fn(_: u8) -> bool` and `fn(x: u8) -> bool` are the type `fn(u8) -> bool`.
//!
//! The Reference, "Function pointer types": `MaybeNamedParam: OuterAttribute* ( ( IDENTIFIER
//! | _ ) : )? Type`. The name is documentation; it is no pattern and introduces no binding.
//!
//! The real site is quickcheck_macros 1.0.0 src/lib.rs. `#[quickcheck] fn p(data: Vec<u8>) ->
//! bool { .. }` expands to `#[test] fn p() { fn p(data: Vec<u8>) -> bool { .. }
//! ::quickcheck::quickcheck(p as fn(_: Vec<u8>) -> bool) }`: each input becomes `_: #ty`. In
//! turso c1851p there are 66 bodies like this (core/numeric/decimal.rs
//! `prop_blob_decode_no_panic`, core/storage/sqlite3_ondisk.rs
//! `varint_len_matches_write_varint`, ..), each "check stopped: .. UnknownInCleanBody { at:
//! PatTy(PatId(0)) }".
//!
//! The cause, read in blaze-lower src/machine.rs:
//! - `FN_PTR_TYPE` takes its parameters as `L::Params`, the list a fn's parameters use.
//! - Each `PARAM`'s pattern child is lowered like any pattern: `WILDCARD_PAT` gives
//!   `put_pat(Pat::Wild)`, and `IDENT_PAT` gives a binding that is put in scope.
//! - So the body holds a pattern no expression owns. The checker never types it, and
//!   writeback's invariant refuses the clean body.
//! The nested `fn p` is skipped by `should_skip` (an item in a `STMT_LIST`), so the outer
//! body's only pattern, PatId(0), is the cast type's `_`.
//!
//! The fix is a clause in blaze-lower's `should_skip` (machine.rs) that skips a fn pointer
//! PARAM's pattern child. CONTROL: with that clause taken out, `wild` is "not checked: ..
//! UnknownInCleanBody" again.
//!
//! The cases:
//! - `wild`: quickcheck's shape. It must be clean, and is "not checked: .. UnknownInCleanBody"
//!   before the fix.
//! - `named`: the same type with a name, which must also be clean.
//! - `leak` (the twin rustc refuses): `x` named only in `fn(x: u8)` is not in scope after it,
//!   so this is E0425. Before the fix the IDENT_PAT's binding could resolve it.
//! - `param` (CONTROL): the fn's own parameter `x` beside such a type is clean either way.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_items.rs.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::lower_made::{LowerTarget, lower_made};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store: its
/// name and its outcome ("clean", the diagnostics, or the failure as printed). The harness of
/// tests/front2_items.rs.
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let outcome = match lower_made(&mut records, prepared, LowerTarget::Free(k)) {
            Err(e) => format!("not lowered: {e}"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                let scope = body.scope(&spell);
                match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => "clean".to_string(),
                    Ok(done) => {
                        let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}: `{}`", d.code.code(), d.at, text.get(d.at.start as usize..d.at.end as usize).unwrap_or("?"))).collect();
                        format!("diags {d:?}")
                    }
                    Err(e) => format!("not checked: {e}"),
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// Expected lines: "fn ptr names: wild: clean", "fn ptr names: named: clean", "fn ptr names:
/// leak: diags [\"E0425 at ..\"]", "fn ptr names: param: clean".
#[test]
fn fn_pointer_parameter_names_bind_nothing() {
    const TEXT: &str = "fn prop(v: u8) -> bool { v == 0 }
fn run(f: fn(u8) -> bool) -> bool { f(0) }
fn wild() -> bool {
    fn prop(v: u8) -> bool { v == 0 }
    run(prop as fn(_: u8) -> bool)
}
fn named() -> bool { run(prop as fn(x: u8) -> bool) }
fn leak() -> u8 { let _f: fn(x: u8) -> bool = prop; x }
fn param(x: u8) -> u8 { let _f: fn(x: u8) -> bool = prop; x }
fn main() {}
";
    let got = check_fns(TEXT, &["wild", "named", "leak", "param"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("fn ptr names: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    assert_eq!(outcome("wild"), "clean", "{}", lines.join("\n"));
    assert_eq!(outcome("named"), "clean", "{}", lines.join("\n"));
    assert!(outcome("leak").contains("E0425"), "{}", lines.join("\n"));
    assert_eq!(outcome("param"), "clean", "{}", lines.join("\n"));
}
