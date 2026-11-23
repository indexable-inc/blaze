//! Order independence of the front end's checker: the result of checking a body does not depend
//! on what was checked before it on the same store. The store holds the world's records (item
//! heads, their predicate tables), which every body reads; nothing a body's check makes (its
//! inference variables, a solver answer in canonical form) may outlive that check. Each body is
//! checked ALONE (a fresh world and store), then all of them in three stated orders on ONE store:
//! every body's outcome (clean, its findings, or its failure) must be equal in all four.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// For each run of `runs`: each fn of the run in the crate `text`, lowered and checked in order
/// over ONE front end and store made fresh for that run (the resolved names are shared: they
/// are the program's, made before any check): its name and its outcome ("clean", the
/// diagnostics, or the failure as printed).
fn check_runs(text: &str, runs: &[&[&str]]) -> Vec<Vec<(String, String)>> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/order/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "order", dir: "/order/", root: "/order/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let mut out = Vec::new();
    for fns in runs {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
            let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
        let (mut buckets, mut consts) = (None, Consts::default());
        let mut got = Vec::new();
        for &n in *fns {
            let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
                .unwrap_or_else(|e| panic!("`{n}`: {e}"));
            let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
            let outcome = match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(e) => format!("not lowered: {e}"),
                Ok(body) => {
                    let view = body.lowered.view();
                    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                    let scope = body.scope(&spell);
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    match check_body(&mut records, &body, &scope) {
                        Ok(done) if done.diags.is_empty() => "clean".to_string(),
                        Ok(done) => {
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
                            format!("diags {d:?}")
                        }
                        Err(e) => format!("not checked: {e}"),
                    }
                }
            };
            got.push((n.to_string(), outcome));
        }
        out.push(got);
    }
    out
}

/// The census's largest class, reduced by the items lane (tests/front2_items.rs `sort_probe`):
/// `a? << 4` checked after a `for` over `chunks_exact(2).enumerate()` on one store.
const PAIR: &str = "fn chunks(hex: &[u8]) -> usize { let mut n = 0; for (at, _p) in hex.chunks_exact(2).enumerate() { n = at; } n }
fn question(a: Option<u8>) -> Option<u8> { let b = a? << 4; Some(b) }
fn main() {}
";

#[test]
fn shift_of_question_after_enumerate() {
    let got = check_runs(PAIR, &[&["question"], &["chunks", "question"], &["question", "chunks"]]);
    let (alone, after, before) = (&got[0], &got[1], &got[2]);
    let shown = format!(
        "ORDER question alone: {:?}\nORDER chunks then question: {:?} / {:?}\nORDER question then chunks: {:?} / {:?}",
        short(&alone[0].1),
        short(&after[0].1),
        short(&after[1].1),
        short(&before[0].1),
        short(&before[1].1)
    );
    println!("{shown}");
    assert!(alone[0].1 == after[1].1 && after[0].1 == before[1].1, "a body's outcome depends on what was checked before it:\n{shown}");
}

/// Twelve small bodies of mixed kinds: a `?`, a shift, a `for` over an iterator adaptor, a
/// closure passed to `map`, methods on a slice, a const-generic array, a `match`, an index
/// assignment, a closure with a range pattern, a `for` over an array, a `sum` over a map.
const MIXED: &str = "fn question(a: Option<u8>) -> Option<u8> { let b = a? << 4; Some(b) }
fn chunks(hex: &[u8]) -> usize { let mut n = 0; for (at, _p) in hex.chunks_exact(2).enumerate() { n = at; } n }
fn shl(a: u8) -> u8 { a << 4 }
fn map_add(a: Option<u8>) -> Option<u16> { a.map(|x| x as u16 + 1) }
fn slice_len(s: &[u8]) -> usize { s.len() + s.iter().count() }
fn arr<const N: usize>(a: [u8; N]) -> usize { a.len() }
fn arr_call() -> usize { arr([1u8, 2, 3]) }
fn pick(x: Option<u8>) -> u8 { match x { Some(v) if v > 3 => v, Some(_) => 1, None => 0 } }
fn q1(a: Option<u8>) -> Option<u8> { let b = a?; Some(b) }
fn index_mut(at: usize) -> u8 { let mut id = [0u8; 32]; id[at] = 1; id[0] }
fn digit(x: u8) -> Option<u8> { let d = |byte: u8| match byte { b'0'..=b'9' => Some(byte - b'0'), _ => None }; d(x) }
fn total(v: &[u32]) -> u32 { v.iter().map(|x| x * 2).sum() }
fn main() {}
";

/// An outcome without the parts every line repeats (the body's key, the source file's path), so
/// that a line of the report shows the refusal itself.
fn short(o: &str) -> String {
    let o = o.find("Malformed(").map_or(o, |at| &o[at..]);
    o.replace("Location { file: \"crates/", "").replace("Some(BadPred { kind: ", "(").chars().take(220).collect()
}

const BODIES: [&str; 12] = ["question", "chunks", "shl", "map_add", "slice_len", "arr", "arr_call", "pick", "q1", "index_mut", "digit", "total"];

#[test]
fn outcome_does_not_depend_on_order() {
    let forward: Vec<&str> = BODIES.to_vec();
    let backward: Vec<&str> = BODIES.iter().rev().copied().collect();
    let evens_first: Vec<&str> = BODIES.iter().step_by(2).chain(BODIES.iter().skip(1).step_by(2)).copied().collect();
    let mut runs: Vec<&[&str]> = BODIES.chunks(1).collect();
    runs.extend([&forward[..], &backward[..], &evens_first[..]]);
    let mut got = check_runs(MIXED, &runs);
    let orders = got.split_off(BODIES.len());
    let alone: Vec<(String, String)> = got.into_iter().map(|mut r| r.remove(0)).collect();
    let mut differ = Vec::new();
    for (label, order) in ["forward", "backward", "evens first"].into_iter().zip(orders) {
        for (n, got) in order {
            let want = &alone.iter().find(|(m, _)| *m == n).expect("each body checked alone").1;
            if &got != want {
                differ.push(format!("{label}: `{n}`: alone {:?}, here {:?}", short(want), short(&got)));
            }
        }
    }
    for (n, o) in &alone {
        println!("ORDER alone `{n}`: {o}");
    }
    for d in &differ {
        println!("ORDER differs {d}");
    }
    println!("ORDER {} bodies, 3 orders on one store, {} outcomes differ from alone", BODIES.len(), differ.len());
    let alone_shown: Vec<String> = alone.iter().map(|(n, o)| format!("`{n}`: {}", o.chars().take(160).collect::<String>())).collect();
    assert!(differ.is_empty(), "{} outcomes depend on what was checked before:\n{}\nalone:\n{}", differ.len(), differ.join("\n"), alone_shown.join("\n"));
}

/// Impls written in blocks (tests/front2_nested.rs): `via_const` and `via_fn` are answered by
/// impls in another item's block, `via_module` by a module-level impl (the control: all three
/// have one outcome); `holder` is the fn whose body writes one of the impls.
const NESTED: &str = "trait Tr { fn f(&self) -> u8; }
struct A;
struct B;
struct M;
const _: () = { use crate::Tr as T2; impl T2 for A { fn f(&self) -> u8 { 1 } } };
fn holder() -> u8 { impl Tr for B { fn f(&self) -> u8 { 2 } } 0 }
impl Tr for M { fn f(&self) -> u8 { 3 } }
fn via_const(x: A) -> u8 { x.f() }
fn via_fn(x: B) -> u8 { x.f() }
fn via_module(x: M) -> u8 { x.f() }
fn main() {}
";

const NESTED_BODIES: [&str; 4] = ["via_const", "holder", "via_fn", "via_module"];

/// An impl in a block answers the same whether the body that writes it was checked before the
/// body that uses it, after it, or never.
#[test]
fn an_impl_in_a_block_does_not_depend_on_order() {
    let forward: Vec<&str> = NESTED_BODIES.to_vec();
    let backward: Vec<&str> = NESTED_BODIES.iter().rev().copied().collect();
    let mut runs: Vec<&[&str]> = NESTED_BODIES.chunks(1).collect();
    runs.extend([&forward[..], &backward[..]]);
    let mut got = check_runs(NESTED, &runs);
    let orders = got.split_off(NESTED_BODIES.len());
    let alone: Vec<(String, String)> = got.into_iter().map(|mut r| r.remove(0)).collect();
    let of = |n: &str| alone.iter().find(|(m, _)| m == n).expect("each body checked alone").1.clone();
    let mut differ = Vec::new();
    for (label, order) in ["forward", "backward"].into_iter().zip(orders) {
        for (n, got) in order {
            if got != of(&n) {
                differ.push(format!("{label}: `{n}`: alone {:?}, here {:?}", short(&of(&n)), short(&got)));
            }
        }
    }
    for (n, o) in &alone {
        println!("NESTED ORDER alone `{n}`: {}", short(o));
    }
    println!("NESTED ORDER {} bodies, 2 orders on one store, {} outcomes differ from alone", NESTED_BODIES.len(), differ.len());
    assert!(differ.is_empty(), "{} outcomes depend on what was checked before:\n{}", differ.len(), differ.join("\n"));
    let control = of("via_module");
    assert_eq!((of("via_const"), of("via_fn")), (control.clone(), control), "an impl in a block answers as a module-level impl does");
}
