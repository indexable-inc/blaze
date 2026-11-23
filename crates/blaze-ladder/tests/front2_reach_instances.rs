//! The reach walk's instance questions (front2/reach/instances.rs) are asked at the self types
//! the walk meets, never as every trait fn times every shape the program's bodies hold (the
//! 457 MB holder of a hello world's `blaze run`: port27/hello-1116/TABLES-NOTES.md, "Peak
//! attribution", holder 1). Three programs, each walked from its `run`:
//!
//! (a) `trait Three` with three fns and four impls (`A`, `B`, `C`, `D`); `via<T: Three>` calls
//!     `t.one()` (decided at the instance), called at two sites, `via(&A)` and `via(&B)`. The
//!     Reference, "Generic functions" and "Trait and lifetime bounds": the fn an instance runs is
//!     the one of the impl for its type argument, so the callees are `A::one` and `B::one`;
//!     `C` and `D` are written in no body, `two` and `three` are called nowhere. The pairs asked
//!     of `Three` are `one` at `A`, at `B` and at `Param` (the blanket impls' question); the
//!     product asked `one` at every shape the reached bodies hold (hundreds, std's bodies
//!     included), which the bound on `one`'s pairs fails.
//!     Line it prints (and asserts): `instances: reached A::one B::one; pairs one@A one@B
//!     one@Param`.
//! (b) `trait Speak` with impls for `A`, `B`, `C`; `call(s: &dyn Speak)` calls `s.speak()`
//!     (decided at the object); `run` coerces `&A` and `&B` to `&dyn Speak` and writes a `C` it
//!     never coerces. The Reference, "Trait objects": "the vtable ... for the concrete type"
//!     is made where the pointer is unsized; so the impl fns reached are `A::speak` and
//!     `B::speak`, not `C::speak` (which a question at every met shape reaches).
//!     Line it prints (and asserts): `dyn: reached A::speak B::speak; pairs speak@A speak@B`.
//! (c) tests/front2_reach_statics.rs's program: its reached set is unchanged.
//!     Line it prints (and asserts): `statics: a Checked, b Checked, c Checked`.
//! (d) `trait Greet` with a PROVIDED fn `hi`; `A`'s impl overrides it, `B`'s leaves it out.
//!     `run` calls `B.hi()` (a concrete call: the trait's own body runs, the Reference,
//!     Traits, "Associated functions") and `via(&A)`, where `via<T: Greet>` calls `t.hi()`
//!     (decided at the instance). So `Greet::hi` is walked as a body AND decided at an
//!     instance, and the instance `via::<A>` runs `A::hi`. Before, the walk's pop dropped the
//!     instance decision of a fn already walked as a body (or replaced it by `Checked`), so
//!     `hi` was never asked at `A` and `A::hi` was never reached: the hello path's stubs
//!     "`integer_min` at [Uint(Usize)] reaches item .. which no fn row tables" (core's
//!     `<impl PartialOrd for usize>::lt`, `PartialOrd::lt` being a body through every derived
//!     `PartialOrd`). A wrong version prints `provided: reached ; pairs `.
//!     Line it prints (and asserts): `provided: reached A::hi; pairs hi@A hi@B hi@Param`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_clone.rs
//! does. NOT RUN by the writer (a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::Origin;
use blaze_ladder::front2::reach::{First, Reached, reach};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::{ItemKey, PNode, Shape};

const THREE: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}

trait Three {
    fn one(&self) -> i32;
    fn two(&self) -> i32;
    fn three(&self) -> i32;
}

struct A;
struct B;
struct C;
struct D;

impl Three for A {
    fn one(&self) -> i32 { 1 }
    fn two(&self) -> i32 { 2 }
    fn three(&self) -> i32 { 3 }
}

impl Three for B {
    fn one(&self) -> i32 { 10 }
    fn two(&self) -> i32 { 20 }
    fn three(&self) -> i32 { 30 }
}

impl Three for C {
    fn one(&self) -> i32 { 100 }
    fn two(&self) -> i32 { 200 }
    fn three(&self) -> i32 { 300 }
}

impl Three for D {
    fn one(&self) -> i32 { 1000 }
    fn two(&self) -> i32 { 2000 }
    fn three(&self) -> i32 { 3000 }
}

fn via<T: Three>(t: &T) -> i32 {
    t.one()
}

fn run() -> i32 {
    via(&A) + via(&B)
}
";

const SPEAK: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}

trait Speak {
    fn speak(&self) -> i32;
}

struct A;
struct B;
struct C;

impl Speak for A {
    fn speak(&self) -> i32 { 1 }
}

impl Speak for B {
    fn speak(&self) -> i32 { 2 }
}

impl Speak for C {
    fn speak(&self) -> i32 { 3 }
}

fn call(s: &dyn Speak) -> i32 {
    s.speak()
}

fn run() -> i32 {
    let _c = C;
    call(&A) + call(&B)
}
";

const STATICS: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}

fn a() -> i32 {
    20
}

fn b() -> i32 {
    15
}

fn c() -> i32 {
    7
}

static TABLE: [fn() -> i32; 2] = [a, b];

const F: fn() -> i32 = c;

fn run() -> i32 {
    (TABLE[0])() + (TABLE[1])() + F()
}
";

const GREET: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}

trait Greet {
    fn hi(&self) -> i32 {
        0
    }
}

struct A;
struct B;

impl Greet for A {
    fn hi(&self) -> i32 { 1 }
}

impl Greet for B {}

fn via<T: Greet>(t: &T) -> i32 {
    t.hi()
}

fn run() -> i32 {
    B.hi() + via(&A)
}
";

/// What one walk from `run` reached, shown by name.
struct Walked {
    /// Each reached fn of an impl of the trait, `Type::fn`, sorted.
    impl_fns: Vec<String>,
    /// Each pair asked of a fn of the trait whose shape is one of `types` or `Param`, as
    /// `fn@Type`, sorted.
    pairs: Vec<String>,
    /// How many pairs were asked of each fn of the trait, by name, sorted.
    per_fn: Vec<(String, usize)>,
    /// Each of `values`' outcome: `Checked`, another outcome's debug form, or `not reached`.
    outcomes: Vec<String>,
}

/// Walks `src`'s `run` (crate `krate`), naming the impls of `trait_name` by the types `types`.
fn walk(krate: &'static str, src: &'static str, trait_name: Option<&'static str>, types: &[&'static str], values: &[&'static str]) -> Walked {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let dir = format!("/{krate}/");
    let root = format!("/{krate}/src/main.rs");
    let files = [(root.clone(), src.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: krate, dir: &dir, root: &root, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: src }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let lib = |ns: Namespace, name: &'static str| {
        names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let run = lib(Namespace::Values, "run");
    let trait_key: Option<ItemKey> = trait_name.map(|t| front.keys.item(lib(Namespace::Types, t)).unwrap_or_else(|e| panic!("{t}'s key: {e:?}")));
    let type_keys: Vec<(ItemKey, &'static str)> = types.iter().map(|&t| (front.keys.item(lib(Namespace::Types, t)).unwrap_or_else(|e| panic!("{t}'s key: {e:?}")), t)).collect();
    let value_keys: Vec<ItemKey> = values.iter().map(|&v| front.keys.item(lib(Namespace::Values, v)).unwrap_or_else(|e| panic!("{v}'s key: {e:?}"))).collect();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(run)).unwrap_or_else(|e| panic!("run's head: {e:?}"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, run).unwrap_or_else(|e| panic!("run lowers: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("run's spelling");
    let scope = body.scope(&spell);
    let mut checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("run checks: {e:?}"));
    let census = reach(&mut records, prepared, body.owner, First::CheckedBody(&body, &mut checked), &scope);
    let type_name = |k: ItemKey| type_keys.iter().find(|(t, _)| *t == k).map(|(_, n)| *n);
    // A member of an impl of the trait: (its self type's name, its own name).
    let of_impl = |k: ItemKey| -> Option<(String, String)> {
        let Some(Origin::Member { container, name, .. }) = records.keys.origin(k) else { return None };
        let h = records.store.impls.get(container)?;
        if h.trait_ != trait_key || trait_key.is_none() {
            return None;
        }
        let shown = match records.store.tys.view().get(h.self_ty) {
            Some(PNode::Adt(a, _)) => type_name(*a).map_or_else(|| String::from("other"), String::from),
            _ => String::from("other"),
        };
        Some((shown, name.to_string()))
    };
    let mut impl_fns: Vec<String> = census.fns.iter().filter_map(|(k, _)| of_impl(*k)).map(|(t, f)| format!("{t}::{f}")).collect();
    impl_fns.sort();
    impl_fns.dedup();
    let mut pairs = Vec::new();
    let mut per_fn: Vec<(String, usize)> = Vec::new();
    for (f, shape) in census.instance_pairs.iter() {
        let Some(Origin::Member { container, name, .. }) = records.keys.origin(*f) else { continue };
        if Some(*container) != trait_key {
            continue;
        }
        match per_fn.iter_mut().find(|(n, _)| n.as_str() == &**name) {
            Some((_, count)) => *count += 1,
            None => per_fn.push((name.to_string(), 1)),
        }
        let shown = match shape {
            Shape::Param => Some("Param"),
            Shape::Adt(a) => type_name(*a),
            _ => None,
        };
        if let Some(s) = shown {
            pairs.push(format!("{name}@{s}"));
        }
    }
    pairs.sort();
    per_fn.sort();
    let outcomes = value_keys
        .iter()
        .map(|k| match census.fns.iter().find(|(f, _)| f == k).map(|(_, o)| o) {
            Some(Reached::Checked) => String::from("Checked"),
            Some(other) => format!("{other:?}"),
            None => String::from("not reached"),
        })
        .collect();
    Walked { impl_fns, pairs, per_fn, outcomes }
}

/// The most pairs a trait fn decided at an instance may be asked in program (a): its met
/// shapes (`A`, `B`, `Param`) and the few shapes the library files an unread impl at. The
/// product asked it at every shape the reached bodies hold: hundreds.
const MOST_PAIRS_OF_ONE: usize = 16;

#[test]
fn instance_pairs_are_the_met_pairs() {
    let w = walk("three", THREE, Some("Three"), &["A", "B", "C", "D"], &[]);
    let line = format!("instances: reached {}; pairs {}", w.impl_fns.join(" "), w.pairs.join(" "));
    eprintln!("{line}");
    eprintln!("pairs asked per fn of Three: {:?}", w.per_fn);
    assert_eq!(line, "instances: reached A::one B::one; pairs one@A one@B one@Param");
    let one = w.per_fn.iter().find(|(n, _)| n == "one").map_or(0, |(_, c)| *c);
    assert!(one <= MOST_PAIRS_OF_ONE, "`one` was asked at {one} shapes: the product of trait fns and shapes is back");
    assert!(w.per_fn.iter().all(|(n, _)| n == "one"), "a fn of Three no instance calls was asked: {:?}", w.per_fn);
}

#[test]
fn dyn_reaches_only_the_coerced_types() {
    let w = walk("speak", SPEAK, Some("Speak"), &["A", "B", "C"], &[]);
    let line = format!("dyn: reached {}; pairs {}", w.impl_fns.join(" "), w.pairs.join(" "));
    eprintln!("{line}");
    assert_eq!(line, "dyn: reached A::speak B::speak; pairs speak@A speak@B");
}

#[test]
fn provided_fn_walked_as_body_is_still_asked_at_its_instances() {
    let w = walk("greet", GREET, Some("Greet"), &["A", "B"], &[]);
    let line = format!("provided: reached {}; pairs {}", w.impl_fns.join(" "), w.pairs.join(" "));
    eprintln!("{line}");
    // `B` is written in `run`, so `hi` is asked there too (its impl answers with no `hi`).
    assert_eq!(line, "provided: reached A::hi; pairs hi@A hi@B hi@Param");
}

#[test]
fn statics_fixture_reach_unchanged() {
    let w = walk("inits", STATICS, None, &[], &["a", "b", "c"]);
    let line = format!("statics: a {}, b {}, c {}", w.outcomes[0], w.outcomes[1], w.outcomes[2]);
    eprintln!("{line}");
    assert_eq!(line, "statics: a Checked, b Checked, c Checked");
}
