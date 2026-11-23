//! A struct expression's or struct pattern's path is resolved in the TYPE namespace (blaze-lower
//! typeck.rs `PathNs`, `TypeckBody::path_ns`; front2 world_paths.rs `path`).
//!
//! The real site is turso core/io/unix.rs:132 and :187: `let mut flock = libc::flock { l_type: ..,
//! .. };`. libc 0.2.191 declares both `pub struct flock` (src/unix/bsd/apple/mod.rs:588) and
//! `pub fn flock(fd, operation)` (src/unix/mod.rs:2273). turso c1851p gave "about `flock`
//! (E0574 NotAStruct)" at both sites. Every body path was resolved in `Ns::Value`, so the path
//! found the fn.
//!
//! The rule is rustc_resolve late.rs `PathSource::namespace`. `PathSource::Struct` is `TypeNS`.
//! `Expr`, `Pat` and `TupleStruct` are `ValueNS`. The Reference, "Namespaces": a struct is in the
//! type namespace and a function in the value namespace, so the two `flock`s coexist.
//!
//! The cases:
//! - `a` is turso's shape, a struct expression `flock { .. }` beside `fn flock`: clean.
//! - `p` is the struct-pattern form, `let flock { l_len } = f`: clean.
//! - `b` is the CONTROL. The call `flock(1)` is a path expression, Value, so it is the fn: clean.
//! - `t` is the twin rustc refuses. `only_fn { .. }` names only a fn, so it is E0574 ("expected
//!   struct, variant or union type, found function"). The Type walk misses at the last segment
//!   and falls back to Values (resolve.rs `path_with`).
//! - These rows stay Value: a unit struct as a value (`u`), a tuple-struct constructor call (`k`)
//!   and a tuple-struct pattern (`kp`, rustc `PathSource::TupleStruct`). All are clean. They are
//!   controls that the Value sites still resolve. A unit or tuple struct is in both namespaces,
//!   so no fn can share its name to make them discriminate.
//!
//! CONTROL (rule 23): with every `PathNs::Type` in blaze-lower typeck.rs `path` replaced by
//! `PathNs::Value`, `a` and `p` are E0574 again, and the other rows do not change.
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

/// turso is a Rust 2024 crate.
const EDITION: &str = "2024";

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

/// Expected lines: "struct path ns: a: clean", "p: clean", "b: clean", "t: diags [\"E0574 at
/// ..\"]", "u: clean", "k: clean", "kp: clean".
#[test]
fn a_struct_path_is_resolved_in_the_type_namespace() {
    const TEXT: &str = "#[allow(non_camel_case_types)]
pub struct flock { pub l_len: i64 }
pub fn flock(_: i32) -> i32 { 0 }
pub fn only_fn(_: i32) -> i32 { 0 }
pub struct U;
pub struct K(pub i64);
fn a() -> i64 { let f = flock { l_len: 1 }; f.l_len }
fn p(f: flock) -> i64 { let flock { l_len } = f; l_len }
fn b() -> i32 { flock(1) }
fn t() -> i64 { let _ = only_fn { l_len: 1 }; 0 }
fn u() -> U { U }
fn k() -> K { K(1) }
fn kp(v: K) -> i64 { let K(x) = v; x }
fn main() {}
";
    let got = check_fns(TEXT, &["a", "p", "b", "t", "u", "k", "kp"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("struct path ns: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    for n in ["a", "p", "b", "u", "k", "kp"] {
        assert_eq!(outcome(n), "clean", "`{n}`:\n{}", lines.join("\n"));
    }
    assert!(outcome("t").contains("E0574"), "`t`: want E0574 for a fn named as a struct:\n{}", lines.join("\n"));
}
