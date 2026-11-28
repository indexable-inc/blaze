//! `blaze check`'s BORROWS stage entry (front2/borrows.rs, `ProjectWorld::check_borrows`) on a
//! copy of the owner's hello world with four more bodies, each checked by the real front end,
//! lowered to its flow (blaze-flow-lower) and move and borrow checked (blaze-flow).
//!
//! - `moved`: the Reference, "Moved and copied types": "When a place expression is evaluated
//!   in a value expression context ... If the type of that value implements Copy, then the
//!   value will be copied. In the remaining situations ... the value is moved"; and
//!   "Variables": a moved-from variable cannot be read until reinitialized. `Token` is not
//!   `Copy`, so `let c = a;` after `let b = a;` uses a moved value: ONE finding, E0382, whose
//!   range covers the second `a`.
//! - `both`: the Reference, "References and borrowing" (Pointer types, "Mutable references"):
//!   "&mut ... references ... a unique reference": while the shared `r = &c` is live (it is
//!   read after), `&mut c` is refused: ONE finding, E0502, whose range covers `&mut c`.
//! - `clean`: a move then a use of the new owner, nothing borrowed across a write: an empty
//!   verdict, and not refused.
//! - `with_closure`: a body with a closure: the closure's body is checked as its own flow
//!   (blaze-flow-lower `lower_closure`, front2 borrows_nested.rs) and the call of it is a
//!   shared use of the closure value: an empty verdict, and not refused.
//!
//! Prints, per body, a line `borrows <name>: <findings> refused <refusal>`; the expected ones:
//! "borrows moved: [E0382 at \"a\"] refused None", "borrows both: [E0502 at \"&mut c\"] refused
//! None", "borrows clean: [] refused None", "borrows with_closure: [] refused None". Needs the
//! installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowVerdict;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

/// His `src/main.rs` as written, plus the four bodies.
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}

struct Token {
    n: u8,
}

fn moved() {
    let a = Token { n: 1 };
    let b = a;
    let c = a;
}

fn both() {
    let mut c = Token { n: 1 };
    let r = &c;
    let m = &mut c;
    let k = r;
}

fn clean() {
    let a = Token { n: 1 };
    let b = a;
    let n = b.n;
}

fn with_closure() {
    let f = || 1u8;
    let x = f();
}
";

/// One body's verdict as the line the test prints: each finding's code and the text its range
/// covers in the body's item text.
fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

#[test]
fn own_bodies_move_and_borrow_checked() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
    let text_of = |b: BodyRef<'_>| {
        let k = match b {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, .. } => container,
        };
        names.item_text(k).expect("the body's item text").text
    };

    // A use after a move: one E0382 at the second `a`.
    let moved = find("moved");
    let (types, v) = world.check_borrows(moved, heap.alloc());
    let item = text_of(moved);
    println!("borrows moved: {}", shown(item, &v));
    assert!(types.is_clean(), "moved's types: {types:?}");
    assert!(v.refused.is_none(), "moved: {v:?}");
    assert_eq!(v.findings.len(), 1, "moved: {}", shown(item, &v));
    let d = v.findings[0];
    assert_eq!(d.code, "E0382", "moved: {}", shown(item, &v));
    let second = item.find("let c = a").expect("the second use") as u32 + "let c = ".len() as u32;
    assert!(d.start <= second && d.end >= second + 1, "the range {}..{} of {item:?} is not the second `a` at {second}", d.start, d.end);

    // A mutable borrow while a shared one is live: one E0502 at `&mut c`.
    let both = find("both");
    let (types, v) = world.check_borrows(both, heap.alloc());
    let item = text_of(both);
    println!("borrows both: {}", shown(item, &v));
    assert!(types.is_clean(), "both's types: {types:?}");
    assert!(v.refused.is_none(), "both: {v:?}");
    assert_eq!(v.findings.len(), 1, "both: {}", shown(item, &v));
    let d = v.findings[0];
    assert_eq!(d.code, "E0502", "both: {}", shown(item, &v));
    let covered = item.get(d.start as usize..d.end as usize).unwrap_or("");
    assert!(covered.contains("&mut c"), "the range {}..{} of {item:?} is {covered:?}, not `&mut c`", d.start, d.end);

    // A clean body: checked to its end, no finding.
    let clean = find("clean");
    let (types, v) = world.check_borrows(clean, heap.alloc());
    println!("borrows clean: {}", shown(text_of(clean), &v));
    assert!(types.is_clean(), "clean's types: {types:?}");
    assert!(v.is_clean(), "clean: {v:?}");

    // A body with a closure: the closure's body checked on its own; clean.
    let closure = find("with_closure");
    let (types, v) = world.check_borrows(closure, heap.alloc());
    println!("borrows with_closure: {}", shown(text_of(closure), &v));
    assert!(types.is_clean(), "with_closure's types: {types:?}");
    assert!(v.is_clean(), "with_closure: {v:?}");
}

const ASYNC_AND_TYPE_OUTLIVES_MAIN: &str = "fn main() {}

struct Guard<'a, T>(&'a T);

struct InnerLock<T>(T);

impl<T> InnerLock<T> {
    async fn inner_lock(&self) -> Guard<'_, T> {
        Guard(&self.0)
    }
}

struct Lock<T> {
    inner: InnerLock<T>,
}

impl<T> Lock<T> {
    async fn lock(&self) -> Guard<'_, T> {
        self.inner.inner_lock().await
    }
}

fn need_static<T: 'static>(t: T) {
    let _ = t;
}

fn hold<'a, T: 'a>(t: T, r: &'a u8) -> &'a u8 {
    let _ = t;
    r
}

async fn async_bad_param<T>(t: T) {
    need_static(t);
}

async fn async_bad_guard<'a, 'b, T>(x: &'a Lock<T>) -> Guard<'b, T> {
    Guard(&x.inner.0)
}

async fn async_bad_static_ref<'a, T>(x: &'a Lock<T>) -> &'static T {
    &x.inner.0
}

struct Run<'a>(&'a str);

impl<'a> Run<'a> {
    fn at_named<'k>(&self) -> &'k str
    where
        Self: 'k,
    {
        self.0
    }

    async fn at_async<'k>(&self) -> &'k str
    where
        Self: 'k,
    {
        self.0
    }

    fn at_missing_bound<'k>(&self) -> &'k str {
        self.0
    }
}

impl Run<'_> {
    fn at_elided<'k>(&self) -> &'k str
    where
        Self: 'k,
    {
        self.0
    }
}

struct GenericRun<'a, T>(&'a T);

impl<'a, T> GenericRun<'a, T> {
    fn generic_get<'k>(&self) -> &'k T
    where
        Self: 'k,
    {
        self.0
    }

    fn generic_hold<'k>(&self, r: &'k u8) -> &'k u8
    where
        Self: 'k,
    {
        hold(self.0, r)
    }
}

fn tuple_outlives<'a, 'b, 'k>(x: (&'a str, &'b str)) -> (&'k str, &'k str)
where
    (&'a str, &'b str): 'k,
{
    x
}

trait Proj<'a> {
    type Assoc;
}

impl<'a> Proj<'a> for () {
    type Assoc = u8;
}

fn proj_outlives_no_region<'a, 'k>(x: &'a str) -> &'k str
where
    <() as Proj<'a>>::Assoc: 'k,
{
    x
}

fn hr_fn_ptr_no_region<'a, 'k>(x: &'a str) -> &'k str
where
    for<'b> fn(&'b str): 'k,
{
    x
}

fn hr_fn_ptr_free_region<'a, 'k>(x: &'a str) -> &'k str
where
    for<'b> fn(&'b str) -> &'a str: 'k,
{
    x
}

trait UnusedProj<'a> {
    type Assoc;
}

impl<'a> UnusedProj<'a> for () {
    type Assoc = u8;
}

trait UnusedDyn<'a> {}

fn proj_sibling_yields_ref<'a, 'b, 'k, U>(x: &'b U) -> &'k U
where
    (<() as UnusedProj<'a>>::Assoc, &'b U): 'k,
{
    x
}

fn proj_sibling_yields_param<'a, 'b, 'k, U>(u: U, r: &'k u8) -> &'k u8
where
    (<() as UnusedProj<'a>>::Assoc, &'b U): 'k,
{
    hold(u, r)
}

fn proj_sibling_does_not_yield_proj_region<'a, 'b, 'k, U>(x: &'a str, _u: &'b U) -> &'k str
where
    (<() as UnusedProj<'a>>::Assoc, &'b U): 'k,
{
    x
}

fn dyn_sibling_yields_ref<'a, 'b, 'k, U>(x: &'b U) -> &'k U
where
    (&'a (dyn UnusedDyn<'a> + 'a), &'b U): 'k,
{
    x
}

fn dyn_object_lifetime_yields_bound<'a, 'b, 'k>(x: &'b str) -> &'k str
where
    (dyn UnusedDyn<'a> + 'b): 'k,
{
    x
}

fn dyn_predicate_lifetime_does_not_yield_bound<'a, 'k>(x: &'a str) -> &'k str
where
    (dyn UnusedDyn<'a> + 'static): 'k,
{
    x
}
";

#[test]
fn async_implied_bounds_and_concrete_type_outlives() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/async-outlives/src/main.rs".to_string(), ASYNC_AND_TYPE_OUTLIVES_MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "async_outlives", dir: "/async-outlives/", root: "/async-outlives/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: ASYNC_AND_TYPE_OUTLIVES_MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
    let text_of = |b: BodyRef<'_>| {
        let k = match b {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, .. } => container,
        };
        names.item_text(k).expect("the body's item text").text
    };

    // Positive cases: async fn implied bounds + concrete TypeOutlives (`where Self: 'k`, tuples, free region under `for<'b> fn`).
    for name in [
        "inner_lock",
        "lock",
        "at_named",
        "at_async",
        "at_elided",
        "generic_get",
        "generic_hold",
        "tuple_outlives",
        "hr_fn_ptr_free_region",
        "proj_sibling_yields_ref",
        "proj_sibling_yields_param",
        "dyn_sibling_yields_ref",
        "dyn_object_lifetime_yields_bound",
    ] {
        let b = find(name);
        let (types, v) = world.check_borrows(b, heap.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows {name}: {line}");
        assert!(types.is_clean(), "{name}'s types: {types:?}");
        assert!(v.is_clean(), "{name}: {line}");
    }

    // Negative control: unbounded `T` in an `async fn` passed to `T: 'static` must produce E0310 (not generic "outlives").
    {
        let b = find("async_bad_param");
        let (types, v) = world.check_borrows(b, heap.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows async_bad_param: {line}");
        assert!(types.is_clean(), "async_bad_param's types: {types:?}");
        assert!(v.refused.is_none(), "async_bad_param: {line}");
        assert!(v.findings.iter().any(|d| d.code == "E0310"), "async_bad_param must report E0310: {line}");
    }

    // Negative controls: missing outlives bounds, projection outlives, and bound-region fn pointer must still fail borrowck.
    for name in [
        "async_bad_guard",
        "async_bad_static_ref",
        "at_missing_bound",
        "proj_outlives_no_region",
        "hr_fn_ptr_no_region",
        "proj_sibling_does_not_yield_proj_region",
        "dyn_predicate_lifetime_does_not_yield_bound",
    ] {
        let b = find(name);
        let (types, v) = world.check_borrows(b, heap.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows {name}: {line}");
        assert!(types.is_clean(), "{name}'s types: {types:?}");
        assert!(v.refused.is_none(), "{name} should be checked, not refused: {line}");
        assert!(!v.findings.is_empty(), "{name} must fail borrowck: {line}");
    }
}
