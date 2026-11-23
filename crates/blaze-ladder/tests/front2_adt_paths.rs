//! A struct's segment in a body path written with fewer arguments than the struct has
//! parameters, on front2 with the real core: tempo's "diag E0107" (tempo-front-10: serde's
//! derive writes `_serde::__private229::Vec::<_serde::__private229::Option<(..)>>::with_capacity(..)`
//! in `visit_map`, "ArgCount { item: .., expected: 2, got: 1 }" against the pinned library's
//! `pub struct Vec<T, A: Allocator = Global>`), reduced.
//!
//! The Reference, "Paths", "Paths in expressions" gives this form as its own example of the
//! turbofish: "`Vec::<u8>::with_capacity(1024);`". It does not say how the left-out `A` is
//! filled; blaze fills it with the default as a type path does (the Reference, "Generic
//! parameters", and front2 types.rs `with_defaults`: `Vec<u8>` is `Vec<u8, Global>`), see
//! src/front2/adt_paths.rs. What stays an error: more arguments than parameters (E0107), a
//! left-out parameter with no default (E0107 in a body path; a refusal naming "no default"
//! in a type), as the Reference, "Generic parameters" and "Paths" allow no other reading.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_panics.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const PATHS: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
struct G;
struct H2;
trait Mk { fn mk() -> Self; }
impl Mk for G { fn mk() -> Self { G } }
impl Mk for H2 { fn mk() -> Self { H2 } }
struct V<T, A = G> { t: T, a: A }
impl<T> V<T, G> {
    fn new(t: T) -> Self { V { t: t, a: G } }
}
impl<T, A: Mk> V<T, A> {
    fn make(t: T) -> Self { V { t: t, a: A::mk() } }
}
struct P<T, U> { t: T, u: U }
impl<T, U> P<T, U> {
    fn pair(t: T, u: U) -> Self { P { t: t, u: u } }
}
struct W<T> { t: T }
fn c1() -> V<u8, G> { V::<u8>::new(1) }
fn c2() -> V<u8> { V::<u8>::new(1) }
fn c3() -> u8 { let v = V::<u8>::make(1); v.t }
fn c4() -> V<u8, G> { V::new(1) }
fn c5() -> V<u8> { V::<u8> { t: 1, a: G } }
fn c6() -> u8 { let x: V<u8> = V::new(1); x.t }
fn n1() -> V<u8, G> { V::<u8, G, u16>::new(1) }
fn n2() -> P<u8, bool> { P::<u8>::pair(1, true) }
fn n3() -> u8 { let x: W = W { t: 1 }; 1 }
";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Want2 {
    /// Checks with no diagnostic.
    Clean,
    /// The checker's E0107 (`DiagCode::GenericArgCount`).
    Count,
    /// A type written short of a parameter with no default: refused before the check, naming
    /// it (types.rs `with_defaults`: "parameter 0 has no default").
    NoDefault,
}

/// Each body, what it must give, and what it proves.
const BODIES: [(&str, Want2, &str); 9] = [
    ("c1", Want2::Clean, "`V::<u8>::new(1)` is a `V<u8, G>`: the left-out `A` takes its default"),
    ("c2", Want2::Clean, "the same against `V<u8>` in type position (`V<u8, G>`)"),
    ("c3", Want2::Clean, "`V::<u8>::make(1)` with two `Mk` impls: `A` is the default `G`, not a variable left ambiguous"),
    ("c4", Want2::Clean, "`V::new(1)`: nothing written, every parameter inferred"),
    ("c5", Want2::Clean, "`V::<u8> { .. }`: a struct expression's segment takes the default too"),
    ("c6", Want2::Clean, "`let x: V<u8>` in type position is `V<u8, G>`"),
    ("n1", Want2::Count, "`V::<u8, G, u16>`: three arguments for two parameters"),
    ("n2", Want2::Count, "`P::<u8>`: `U` has no default"),
    ("n3", Want2::NoDefault, "`let x: W` in type position: `T` has no default"),
];

/// Expected line: `test adt_segment_takes_its_defaults ... ok`. Without
/// src/front2/adt_paths.rs (the struct branch in `AliasPaths::expand`), `c1`, `c2`, `c3` and
/// `c5` are each "diagnostics [.. GenericArgCount ..]" (E0107, `expected: 2, got: 1`), as on
/// tempo; a fix giving `A` a fresh variable instead leaves `c3` ambiguous; a fix dropping the
/// count check passes `n1` or `n2`.
#[test]
fn adt_segment_takes_its_defaults() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/adp/src/main.rs".to_string(), PATHS.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "adp", dir: "/adp/", root: "/adp/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: PATHS }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for &(name, want, proves) in BODIES.iter() {
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate's root"));
        let lowered = match store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            Err(e) => Err(format!("no head: {e:?}")),
            Ok(_) => lower_fn(&names, prepared, &mut front.keys, &mut store, key).map_err(|e| format!("does not lower: {e}")),
        };
        match (lowered, want) {
            (Ok(b), _) => bodies.push((name, b, want, proves)),
            (Err(e), Want2::NoDefault) if e.contains("no default") => notes.push(format!("{name}: refused, as it must be: {e}")),
            (Err(e), _) => {
                notes.push(format!("{name}: {e} ({proves})"));
                failed.push(name.to_string());
            }
        }
    }
    for (name, body, want, proves) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut records, body, &scope), *want) {
            (Ok(done), Want2::Clean) if done.diags.is_empty() => notes.push(format!("{name}: clean")),
            (Err(CheckFail::Diags { diags, .. }), Want2::Count) if format!("{diags:?}").contains("GenericArgCount") => {
                notes.push(format!("{name}: E0107, as it must be"))
            }
            (Ok(done), _) => {
                notes.push(format!("{name}: Ok with diagnostics {:?}; wanted {want:?} ({proves})", done.diags));
                failed.push(name.to_string());
            }
            (Err(CheckFail::Diags { diags, .. }), _) => {
                notes.push(format!("{name}: diagnostics {diags:?}; wanted {want:?} ({proves})"));
                failed.push(name.to_string());
            }
            (Err(other), _) => {
                notes.push(format!("{name}: {other:?}; wanted {want:?} ({proves})"));
                failed.push(name.to_string());
            }
        }
    }
    assert!(failed.is_empty(), "bodies did not check as they must: {failed:?}\n{}", notes.join("\n"));
}
