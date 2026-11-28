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

trait LateWideProj<'a> {
    type Assoc;
}

impl<'a> LateWideProj<'a> for () {
    type Assoc = u8;
}

fn wide_tuple_late_proj_yields_ref<'a, 'b, 'k, U>(x: &'b U) -> &'k U
where
    (
        &'b U, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, <() as LateWideProj<'a>>::Assoc,
    ): 'k,
{
    x
}

fn wide_tuple_late_proj_yields_param<'a, 'b, 'k, U>(u: U, r: &'k u8) -> &'k u8
where
    (
        &'b U, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, <() as LateWideProj<'a>>::Assoc,
    ): 'k,
{
    hold(u, r)
}

fn wide_tuple_late_proj_does_not_yield_proj_region<'a, 'b, 'k, U>(x: &'a str, _u: &'b U) -> &'k str
where
    (
        &'b U, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, u8,
        u8, u8, u8, u8, u8, u8, u8, u8, u8, <() as LateWideProj<'a>>::Assoc,
    ): 'k,
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
        "wide_tuple_late_proj_yields_ref",
        "wide_tuple_late_proj_yields_param",
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
        "wide_tuple_late_proj_does_not_yield_proj_region",
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

const FUTURE_OUTPUT_SLOTS_MAIN: &str = "use std::future::Future;

fn main() {}

trait Writer {
    fn write(&self) -> i32;
}

trait ErrTrait {
    fn code(&self) -> i32;
}

struct Owned;

impl Writer for Owned {
    fn write(&self) -> i32 {
        1
    }
}

struct Local(i32);

fn read(l: &Local) -> i32 {
    l.0
}

fn move_out(l: Local) -> i32 {
    l.0
}

fn use_writer(w: Box<dyn Writer + 'static>) -> i32 {
    w.write()
}

fn use_ref(r: &i32) -> i32 {
    *r
}

fn block_on<F: Future>(f: F) -> F::Output {
    let _ = f;
    loop {}
}

fn needs_static<F: Future>(f: F) -> F::Output
where
    F::Output: 'static,
{
    let _ = f;
    loop {}
}

async fn within<T>(f: impl Future<Output = T>) -> Result<T, Box<dyn ErrTrait + 'static>> {
    Ok(f.await)
}

fn block_on_dyn_static_local_borrow() -> i32 {
    let local = Local(7);
    let w: Box<dyn Writer + 'static> = block_on(async {
        let _ = read(&local);
        let b: Box<dyn Writer + 'static> = Box::new(Owned);
        b
    });
    let _ = move_out(local);
    use_writer(w)
}

async fn within_async_wrapper_local_borrow() -> Result<i32, Box<dyn ErrTrait + 'static>> {
    let local = Local(7);
    let res = within(async {
        let _ = read(&local);
        42
    })
    .await?;
    let _ = move_out(local);
    Ok(res)
}

fn needs_static_output_local_borrow() -> i32 {
    let local = Local(7);
    let w: Box<dyn Writer + 'static> = needs_static(async {
        let _ = read(&local);
        let b: Box<dyn Writer + 'static> = Box::new(Owned);
        b
    });
    let _ = move_out(local);
    use_writer(w)
}

fn block_on_dangling_drop() -> i32 {
    let local = Local(7);
    let r: &i32 = block_on(async { &local.0 });
    drop(local);
    use_ref(r)
}

fn block_on_move_while_borrowed() -> i32 {
    let local = Local(7);
    let r = block_on(async { &local.0 });
    let _ = move_out(local);
    use_ref(r)
}

fn needs_static_output_captures_local_ref() -> i32 {
    let local = Local(7);
    let r = needs_static(async { &local.0 });
    use_ref(r)
}
";

#[test]
fn async_block_future_output_slots() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/future-output-slots/src/main.rs".to_string(), FUTURE_OUTPUT_SLOTS_MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate {
        name: "future_output_slots",
        dir: "/future-output-slots/",
        root: "/future-output-slots/src/main.rs",
        files: &files,
        edition,
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: FUTURE_OUTPUT_SLOTS_MAIN }];
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

    for name in [
        "block_on_dyn_static_local_borrow",
        "within_async_wrapper_local_borrow",
        "needs_static_output_local_borrow",
    ] {
        let b = find(name);
        let (types, v) = world.check_borrows(b, heap.alloc());
        let line = shown(text_of(b), &v);
        println!("borrows {name}: {line}");
        assert!(types.is_clean(), "{name}'s types: {types:?}");
        assert!(v.is_clean(), "{name}: {line}");
    }

    for name in [
        "block_on_dangling_drop",
        "block_on_move_while_borrowed",
        "needs_static_output_captures_local_ref",
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

const DYN_AND_COPY_MAIN: &str = r#"
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

trait MyErr {}

impl dyn MyErr + 'static {
    fn downcast(&self) -> bool {
        true
    }
}

type Reader = Box<dyn Fn(&(dyn MyErr + 'static)) -> bool + Send + Sync>;

struct Lock<T>(UnsafeCell<T>);
unsafe impl<T: Send> Sync for Lock<T> {}

struct Guard<'a, T>(&'a mut T);

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0
    }
}

impl<T> Lock<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    fn write(&self) -> Guard<'_, T> {
        unsafe { Guard(&mut *self.0.get()) }
    }
    fn try_write(&self) -> Result<Guard<'_, T>, ()> {
        Ok(self.write())
    }
}

static READERS: Lock<Vec<(u32, Reader)>> = Lock::new(Vec::new());

fn dyn_expect_static_local_guard() {
    let mut readers = READERS.write();
    readers.push((
        1u32,
        Box::new(move |link| link.downcast()),
    ));
}

fn dyn_expect_static_chained_unwrap() {
    READERS
        .try_write()
        .unwrap()
        .push((2u32, Box::new(move |link| link.downcast())));
}

fn dyn_expect_param_vec_tuple(v: &mut Vec<(u32, Reader)>) {
    v.push((3u32, Box::new(move |link| link.downcast())));
}

fn dyn_expect_owner_return_ok() -> (u32, Reader) {
    (5u32, Box::new(move |link| link.downcast()))
}

fn dyn_expect_negative_elided(v: &mut Vec<(u32, Box<dyn Fn(&(dyn MyErr + '_)) -> bool>)>) {
    v.push((4u32, Box::new(move |link| link.downcast())));
}

fn dyn_expect_negative_unrelated_owner_param(_other: &Vec<Reader>) {
    let _f: Box<dyn Fn(&(dyn MyErr + '_)) -> bool> = Box::new(move |link| link.downcast());
}

struct Carrier<A, C>(PhantomData<(A, C)>);
fn pick<A, B, C>(p: (A, B), _c: &Carrier<A, C>) -> (A, B) {
    p
}

fn dyn_expect_negative_sibling_other_param() {
    let c: Carrier<u32, Reader> = Carrier(PhantomData);
    let _pair: (u32, Box<dyn Fn(&(dyn MyErr + '_)) -> bool>) =
        pick((0u32, Box::new(move |link| link.downcast())), &c);
}

struct Wrap<T>(T);
fn unrelated_wrap<F>(_f: F) -> Wrap<Reader> {
    loop {}
}

fn dyn_expect_negative_unrelated_wrapper_call() -> Wrap<Reader> {
    unrelated_wrap::<Box<dyn Fn(&(dyn MyErr + '_)) -> bool>>(Box::new(move |link| link.downcast()))
}

fn first_arg<A, B>(a: A, _b: B) -> A {
    a
}

fn dyn_expect_negative_unrelated_sibling() {
    let _f: Box<dyn Fn(&(dyn MyErr + '_)) -> bool> =
        first_arg(Box::new(move |link| link.downcast()), &READERS);
}

struct Sink<T>(PhantomData<T>);
impl<T> Sink<T> {
    fn push(&mut self, _v: T) {}
}

fn unrelated_sink<U, V>(_src: &U) -> Sink<V> {
    Sink(PhantomData)
}

fn dyn_expect_negative_unrelated_unary_call() {
    let mut s = unrelated_sink(&READERS);
    if false {
        let f: Box<dyn Fn(&(dyn MyErr + '_)) -> bool> = Box::new(|_| false);
        s.push(f);
    }
    s.push(Box::new(move |link| link.downcast()));
}

struct OutlivesToken<'a, T>(PhantomData<&'a ()>, PhantomData<T>);
impl<'a, T: 'a> Copy for OutlivesToken<'a, T> {}
impl<'a, T: 'a> Clone for OutlivesToken<'a, T> {
    fn clone(&self) -> Self {
        *self
    }
}

fn copy_outlives_static_scalar_ok() {
    let t: OutlivesToken<'static, u8> = OutlivesToken(PhantomData, PhantomData);
    let a = t;
    let b = t;
    let _ = (a, b);
}

impl<'a, T> OutlivesToken<'a, T> {
    const fn new(_: T) -> Self {
        OutlivesToken(PhantomData, PhantomData)
    }
}

const PROMOTED_OUTLIVES: &'static OutlivesToken<'static, &'static u8> = &OutlivesToken::new(&1u8);
const NON_FREEZE_CELL: &'static core::cell::Cell<u8> = &core::cell::Cell::new(1u8);

struct StaticToken<'a, T>(PhantomData<&'a ()>, PhantomData<T>);
impl<T: Copy> Copy for StaticToken<'static, T> {}
impl<T: Copy> Clone for StaticToken<'static, T> {
    fn clone(&self) -> Self {
        *self
    }
}

fn copy_constrained_static_ok(t: StaticToken<'static, u8>) {
    let a = t;
    let b = t;
    let _ = (a, b);
}

fn copy_constrained_static_err<'short>(t: StaticToken<'short, u8>) {
    let a = t;
    let b = t;
    let _ = (a, b);
}

struct SamePair<'a, 'b, T>(PhantomData<&'a ()>, PhantomData<&'b ()>, PhantomData<T>);
impl<'a, T: Copy> Copy for SamePair<'a, 'a, T> {}
impl<'a, T: Copy> Clone for SamePair<'a, 'a, T> {
    fn clone(&self) -> Self {
        *self
    }
}

fn copy_constrained_same_pair_ok<'a>(p: SamePair<'a, 'a, u8>) {
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn copy_constrained_same_pair_err<'a, 'b>(p: SamePair<'a, 'b, u8>) {
    let a = p;
    let b = p;
    let _ = (a, b);
}

trait Parent<'x> {}
trait Child<'x>: Parent<'x> {}
struct SuperWrap<'a, T>(PhantomData<&'a ()>, PhantomData<T>);
impl<'a, T: Copy + Parent<'a>> Copy for SuperWrap<'a, T> {}
impl<'a, T: Copy + Parent<'a>> Clone for SuperWrap<'a, T> {
    fn clone(&self) -> Self {
        *self
    }
}

fn copy_supertrait_region_ok<'a, T: Copy + Child<'a>>(w: SuperWrap<'a, T>) {
    let a = w;
    let b = w;
    let _ = (a, b);
}

fn copy_supertrait_region_mismatch_err<'a, 'b, T: Copy + Child<'b>>(w: SuperWrap<'a, T>) {
    let a = w;
    let b = w;
    let _ = (a, b);
}

trait Has<'a> {
    type Out;
}
struct ProjToken<'a, T>(PhantomData<&'a ()>, PhantomData<T>);
impl<'a, T: Has<'a, Out = u8>> Copy for ProjToken<'a, T> {}
impl<'a, T: Has<'a, Out = u8>> Clone for ProjToken<'a, T> {
    fn clone(&self) -> Self {
        *self
    }
}

fn copy_proj_region_ok<'a, T: Has<'a, Out = u8>>(p: ProjToken<'a, T>) {
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn copy_proj_region_mismatch_err<'a, 'b, T: Has<'b, Out = u8>>(p: ProjToken<'a, T>) {
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn copy_proj_region_wrong_out_err<'a, T: Has<'a, Out = u16>>(p: ProjToken<'a, T>) {
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn dyn_expect_destructuring_reassign_err(
    s: Vec<Reader>,
    non_static: (Vec<Box<dyn Fn(&(dyn MyErr + '_)) -> bool>>, u32),
) {
    let mut readers = s;
    let _other;
    (readers, _other) = non_static;
    readers.push(Box::new(move |link| link.downcast()));
}

fn copy_outlives_static_ref_ok(t: OutlivesToken<'static, &'static u8>) {
    let a = t;
    let b = t;
    let _ = (a, b);
}

fn copy_outlives_free_fn_bound_ok<'a, 'b: 'a>(t: OutlivesToken<'a, &'b u8>) {
    let a = t;
    let b = t;
    let _ = (a, b);
}

fn copy_outlives_generic_bound_ok<'a, T: 'a>(t: OutlivesToken<'a, T>) {
    let a = t;
    let b = t;
    let _ = (a, b);
}

fn copy_outlives_unproven_err<'short>(_x: &'short u8) {
    let t: OutlivesToken<'static, &'short u8> = OutlivesToken(PhantomData, PhantomData);
    let a = t;
    let b = t;
    let _ = (a, b);
}

fn copy_outlives_unproven_param_err<'short>(t: OutlivesToken<'static, &'short u8>) {
    let a = t;
    let b = t;
    let _ = (a, b);
}

fn copy_outlives_free_fn_missing_bound_err<'a, 'b>(t: OutlivesToken<'a, &'b u8>) {
    let a = t;
    let b = t;
    let _ = (a, b);
}

trait Marker<'a> {}
struct Marked<T>(PhantomData<T>);
impl<T: Marker<'static>> Copy for Marked<T> {}
impl<T: Marker<'static>> Clone for Marked<T> {
    fn clone(&self) -> Self {
        *self
    }
}

fn copy_trait_region_static_ok<T: Marker<'static>>(m: Marked<T>) {
    let a = m;
    let b = m;
    let _ = (a, b);
}

fn copy_trait_region_short_err<'short, T: Marker<'short>>(m: Marked<T>) {
    let a = m;
    let b = m;
    let _ = (a, b);
}

struct MarkedLt<'a, T>(PhantomData<&'a ()>, PhantomData<T>);
impl<'a, T: Copy + Marker<'a>> Copy for MarkedLt<'a, T> {}
impl<'a, T: Copy + Marker<'a>> Clone for MarkedLt<'a, T> {
    fn clone(&self) -> Self {
        *self
    }
}

fn copy_trait_region_param_ok<'a, T: Copy + Marker<'a>>(m: MarkedLt<'a, T>) {
    let a = m;
    let b = m;
    let _ = (a, b);
}

fn copy_trait_region_param_static_ok<T: Copy + Marker<'static>>(m: MarkedLt<'static, T>) {
    let a = m;
    let b = m;
    let _ = (a, b);
}

fn copy_trait_region_param_mismatch_err<'a, 'b, T: Copy + Marker<'b>>(m: MarkedLt<'a, T>) {
    let a = m;
    let b = m;
    let _ = (a, b);
}

fn copy_outlives_match_guard_ok<'a, T: 'a>(t: OutlivesToken<'a, T>) {
    match t {
        x if true => {
            let _ = (x, t);
        }
        _ => {}
    }
}

fn copy_outlives_match_guard_err<'short>(t: OutlivesToken<'static, &'short u8>) {
    match t {
        x if true => {
            let _ = (x, t);
        }
        _ => {}
    }
}

fn copy_outlives_closure_capture_ok<'a, T: 'a>(t: OutlivesToken<'a, T>) {
    let c = move || {
        let _x = t;
    };
    let a = c;
    let b = c;
    let _ = (a, b);
}

fn copy_outlives_closure_capture_err<'short>(t: OutlivesToken<'static, &'short u8>) {
    let c = move || {
        let _x = t;
    };
    let a = c;
    let b = c;
    let _ = (a, b);
}

struct BoxSink<T>(T);
impl<T> BoxSink<T> {
    fn push(&mut self, _v: T) {}
}

fn dyn_expect_ctor_wrap_seed_ok(seed: Reader) {
    let mut w = BoxSink(seed);
    w.push(Box::new(move |link| link.downcast()));
}

struct MethodSink;
impl MethodSink {
    fn put<T>(&mut self, _exemplar: &T, _value: T) {}
}

fn dyn_expect_method_sibling_ok(exemplar: &Reader) {
    let mut s = MethodSink;
    s.put(exemplar, Box::new(move |link| link.downcast()));
}

fn id_res<T, E>(r: Result<T, E>) -> Result<T, E> {
    r
}

fn dyn_expect_match_ok_static_ok<'a>(res: Result<Vec<Reader>, Vec<Box<dyn Fn(&(dyn MyErr + 'a)) -> bool>>>) {
    let mut v = match id_res(res) {
        Ok(v) => v,
        Err(_) => return,
    };
    v.push(Box::new(move |link| link.downcast()));
}

fn dyn_expect_match_err_elided_err(res: Result<Vec<Reader>, Vec<Box<dyn Fn(&(dyn MyErr + '_)) -> bool>>>) {
    let mut v = match id_res(res) {
        Err(v) => v,
        Ok(_) => return,
    };
    v.push(Box::new(move |link| link.downcast()));
}

fn dyn_expect_match_err_short_err<'a>(res: Result<Vec<Reader>, Vec<Box<dyn Fn(&(dyn MyErr + 'a)) -> bool>>>) {
    let mut v = match id_res(res) {
        Err(v) => v,
        Ok(_) => return,
    };
    v.push(Box::new(move |link| link.downcast()));
}

struct Holder<T> {
    readers: T,
}

fn dyn_expect_destructured_generic_param_ok(Holder { mut readers }: Holder<Vec<Reader>>) {
    readers.push(Box::new(move |link| link.downcast()));
}

fn dyn_expect_long_alias_chain_ok(v0: &mut Vec<Reader>) {
    let v1 = v0;
    let v2 = v1;
    let v3 = v2;
    let v4 = v3;
    let v5 = v4;
    let v6 = v5;
    let v7 = v6;
    let v8 = v7;
    let v9 = v8;
    let v10 = v9;
    let v11 = v10;
    let v12 = v11;
    let v13 = v12;
    let v14 = v13;
    let v15 = v14;
    let v16 = v15;
    let v17 = v16;
    let v18 = v17;
    let v19 = v18;
    let v20 = v19;
    let v21 = v20;
    let v22 = v21;
    let v23 = v22;
    let v24 = v23;
    let v25 = v24;
    let v26 = v25;
    let v27 = v26;
    let v28 = v27;
    let v29 = v28;
    let v30 = v29;
    v30.push(Box::new(move |link| link.downcast()));
}

trait Sealed {}
trait Alignment: Sealed {}
trait Invariants {
    type Aliasing;
}

struct Shared;
struct Exclusive;

struct SharedInv;
impl Invariants for SharedInv {
    type Aliasing = Shared;
}

struct ExclusiveInv;
impl Invariants for ExclusiveInv {
    type Aliasing = Exclusive;
}

#[derive(Clone, Copy)]
struct ExclusiveCopyInv;
impl Invariants for ExclusiveCopyInv {
    type Aliasing = Exclusive;
}

impl<A, AA: Alignment> Invariants for (A, AA) {
    type Aliasing = A;
}

struct Ptr<'a, T: ?Sized, I>(&'a T, PhantomData<I>);

impl<'a, T: 'a + ?Sized, I: Invariants<Aliasing = Shared>> Copy for Ptr<'a, T, I> {}
impl<'a, T: 'a + ?Sized, I: Invariants<Aliasing = Shared>> Clone for Ptr<'a, T, I> {
    fn clone(&self) -> Self {
        *self
    }
}

fn copy_proj_shared_ok(x: &u8) {
    let p: Ptr<'_, u8, SharedInv> = Ptr(x, PhantomData);
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn copy_proj_generic_tuple_shared_ok<A: Alignment>(p: Ptr<'_, u8, (Shared, A)>) {
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn copy_proj_exclusive_err(x: &u8) {
    let p: Ptr<'_, u8, ExclusiveInv> = Ptr(x, PhantomData);
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn copy_proj_exclusive_copy_arg_err(x: &u8) {
    let p: Ptr<'_, u8, ExclusiveCopyInv> = Ptr(x, PhantomData);
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn copy_proj_generic_tuple_exclusive_err<A: Alignment>(p: Ptr<'_, u8, (Exclusive, A)>) {
    let a = p;
    let b = p;
    let _ = (a, b);
}

fn main() {}
"#;

#[test]
fn dyn_closure_expectation_and_conditional_copy_projection() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/dyn-copy-test/src/main.rs".to_string(), DYN_AND_COPY_MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate {
        name: "dyn_copy_test",
        dir: "/dyn-copy-test/",
        root: "/dyn-copy-test/src/main.rs",
        files: &files,
        edition,
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: DYN_AND_COPY_MAIN }];
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

    for ok_fn in [
        "dyn_expect_static_local_guard",
        "dyn_expect_static_chained_unwrap",
        "dyn_expect_param_vec_tuple",
        "dyn_expect_owner_return_ok",
        "dyn_expect_ctor_wrap_seed_ok",
        "dyn_expect_method_sibling_ok",
        "dyn_expect_match_ok_static_ok",
        "dyn_expect_destructured_generic_param_ok",
        "dyn_expect_long_alias_chain_ok",
        "copy_proj_shared_ok",
        "copy_proj_generic_tuple_shared_ok",
        "copy_outlives_static_scalar_ok",
        "copy_outlives_static_ref_ok",
        "copy_outlives_free_fn_bound_ok",
        "copy_outlives_generic_bound_ok",
        "copy_outlives_match_guard_ok",
        "copy_outlives_closure_capture_ok",
        "copy_trait_region_static_ok",
        "copy_trait_region_param_ok",
        "copy_trait_region_param_static_ok",
        "copy_constrained_static_ok",
        "copy_constrained_same_pair_ok",
        "copy_supertrait_region_ok",
        "copy_proj_region_ok",
        "PROMOTED_OUTLIVES",
    ] {
        let body = find(ok_fn);
        let (types, v) = world.check_borrows(body, heap.alloc());
        assert!(types.is_clean(), "{ok_fn} types: {types:?}");
        assert!(v.is_clean(), "{ok_fn} borrows: {}", shown(text_of(body), &v));
    }

    // Negative controls: non-`'static` `dyn Fn(&(dyn MyErr + '_))` MUST still fail with E0521
    // (including beside an unrelated `&READERS` sibling arg, unrelated owner param, sibling arg
    // using a different generic parameter, unrelated unary wrapper call, `Err(v)` match arm,
    // or a local reassigned inside a destructuring assignment target).
    for err_fn in [
        "dyn_expect_negative_elided",
        "dyn_expect_negative_unrelated_owner_param",
        "dyn_expect_negative_sibling_other_param",
        "dyn_expect_negative_unrelated_wrapper_call",
        "dyn_expect_negative_unrelated_sibling",
        "dyn_expect_negative_unrelated_unary_call",
        "dyn_expect_match_err_elided_err",
        "dyn_expect_destructuring_reassign_err",
    ] {
        let body = find(err_fn);
        let (types, v) = world.check_borrows(body, heap.alloc());
        assert!(types.is_clean(), "{err_fn} types: {types:?}");
        assert!(v.refused.is_none(), "{err_fn} refused: {:?}", v.refused);
        assert!(
            v.findings.iter().any(|d| d.code == "E0521"),
            "{err_fn} expected E0521, got: {}",
            shown(text_of(body), &v)
        );
    }

    // Named early-bound `'a` in `Err(v)` arm and non-`Freeze` `Cell<u8>` constant promotion
    // must still fail borrowck.
    for err_fn in ["dyn_expect_match_err_short_err", "NON_FREEZE_CELL"] {
        let body = find(err_fn);
        let (types, v) = world.check_borrows(body, heap.alloc());
        assert!(types.is_clean(), "{err_fn} types: {types:?}");
        assert!(v.refused.is_none(), "{err_fn} refused: {:?}", v.refused);
        assert!(
            !v.findings.is_empty(),
            "{err_fn} expected borrow error, got: {}",
            shown(text_of(body), &v)
        );
    }

    // Negative controls: `Ptr` with `Aliasing = Exclusive` (including Copy markers and generic
    // tuples), `OutlivesToken` with unproven `T: 'a` `Copy` bounds (including guards and closure
    // captures), constrained self-lifetime impls (`'static`, `'a/'a`), supertrait regions,
    // regioned projection bounds, and `Marked`/`MarkedLt` mismatches MUST fail with E0382.
    for err_fn in [
        "copy_proj_exclusive_err",
        "copy_proj_exclusive_copy_arg_err",
        "copy_proj_generic_tuple_exclusive_err",
        "copy_outlives_unproven_err",
        "copy_outlives_unproven_param_err",
        "copy_outlives_free_fn_missing_bound_err",
        "copy_outlives_match_guard_err",
        "copy_outlives_closure_capture_err",
        "copy_trait_region_short_err",
        "copy_trait_region_param_mismatch_err",
        "copy_constrained_static_err",
        "copy_constrained_same_pair_err",
        "copy_supertrait_region_mismatch_err",
        "copy_proj_region_mismatch_err",
        "copy_proj_region_wrong_out_err",
    ] {
        let body = find(err_fn);
        let (types, v) = world.check_borrows(body, heap.alloc());
        assert!(types.is_clean(), "{err_fn} types: {types:?}");
        assert!(v.refused.is_none(), "{err_fn} refused: {:?}", v.refused);
        assert!(
            v.findings.iter().any(|d| d.code == "E0382"),
            "{err_fn} expected E0382, got: {}",
            shown(text_of(body), &v)
        );
    }
}
