//! A value path whose first segment names a builtin type, and which fails as a path, is that
//! builtin type with the rest unresolved (front2 world_paths.rs `path`, `builtin_first`).
//!
//! The real site is turso postgres/frontend/session.rs. It has `use std::str;` at line 5,
//! `value.map(str::to_owned)` at lines 2246 and 2314, and `str::to_ascii_lowercase` at line
//! 2372. turso c1851p gave "cannot find value `to_owned` in the item an item the names source
//! holds no path for (key 6275696c74696e000d..) (segment 1) (E0425)".
//!
//! The rule is rustc_resolve late.rs `resolve_qpath`: `PathResult::Module(..) |
//! PathResult::Failed { .. } if (ns == TypeNS || path.len() > 1) &&
//! PrimTy::from_name(path[0].ident.name).is_some()` gives `Res::PrimTy(prim)` with
//! `path.len() - 1` unresolved segments. Its comment says "Such behavior is required for
//! backward compatibility. The same fallback is used when `a` resolves to nothing."
//!
//! The cases:
//! - `a` is turso's shape. It must be clean, and is E0425 at segment 1 before the fix.
//! - `d` is `str::to_ascii_lowercase(v)`, an inherent method of `str` in alloc: clean.
//! - `c` is the twin rustc refuses: `str::nothing()` is E0599 ("no function or associated item
//!   named `nothing` found for type `str`"), never E0425.
//! - `b` is the CONTROL: `str::from_utf8` is the module's fn and still wins. It is clean either
//!   way.
//!
//! CONTROL (rule 23): without the new `if` in the `Err(ResolveError::Path(e))` arm, `a` and `d`
//! are E0425 again and `c` is E0425, not E0599.
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

/// turso is a Rust 2024 crate, with std's `String` and `str` methods. Under `blaze-2026.11`,
/// `std` is bstd (edition/src/lib.rs), whose `String` takes an allocator argument.
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

/// Expected lines: "prim path: a: clean", "prim path: d: clean", "prim path: c: diags
/// [\"E0599 at ..\"]", "prim path: b: clean".
#[test]
fn a_failed_path_from_a_builtin_type_name_is_the_builtin_type() {
    const TEXT: &str = "use std::str;
fn a(v: Option<&'static str>) -> Option<String> { v.map(str::to_owned) }
fn d(v: &str) -> String { str::to_ascii_lowercase(v) }
fn c() { str::nothing() }
fn b() -> bool { str::from_utf8(b\"x\").is_ok() }
fn main() {}
";
    let got = check_fns(TEXT, &["a", "d", "c", "b"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("prim path: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    assert_eq!(outcome("a"), "clean", "{}", lines.join("\n"));
    assert_eq!(outcome("d"), "clean", "{}", lines.join("\n"));
    assert!(outcome("c").contains("E0599") && !outcome("c").contains("E0425"), "{}", lines.join("\n"));
    assert_eq!(outcome("b"), "clean", "{}", lines.join("\n"));
}
