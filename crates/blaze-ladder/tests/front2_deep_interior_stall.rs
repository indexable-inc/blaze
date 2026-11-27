//! Multi-level trait, supertrait-unsizing, and projection obligations that transitively require
//! a local async block's future to be `Send` (2–4 impl where-clause levels deep) stay ambiguous
//! during `select_all()` with solver `why_maybe_chain` leaf `WhyLeaf::InteriorUnknown {
//! coroutine: WhyDef::Expr(block) }`.
//!
//! Each such obligation must be deferred to flow lowering (and decided on the exact witness
//! interior) rather than falling through to E0282/E0283, while negative twins holding an
//! `Rc<i32>` across `.await` must still be rejected with E0277 (never silently accepted and
//! never misclassified as `ForeignWitness` or `INTERIOR_GAP`).
//!
//! - Case A (3-level supertrait unsizing): `Arc<Closure>` -> `Arc<dyn Policy>` where
//!   `trait Policy: Send + Sync + 'static` has no explicit `+ Send` row on `dyn Policy`, and
//!   `impl<F, Fut> Policy for F where F: Fn() -> Fut + Send + Sync + 'static, Fut: Future<Output = ()> + Send + 'static`.
//! - Case B (2-level nested wrapper trait): `Wrapper<ServiceFn<Closure>>: Service` where
//!   `Wrapper<S>: Service where S: Service` and `ServiceFn<F>: Service where Fut: Send`.
//! - Case C (4-level projection through RPIT / `IntoFuture`):
//!   `<Opaque(or_cancel::<F>) as IntoFuture>::Output == Result<i32, ()>` where `OrCancelExt`
//!   requires `F: Future + Send` and `or_cancel` returns `impl Future<Output = Result<F::Output, ()>>`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;

async fn yield_once() {}

// --- Case A: 3-level supertrait unsizing (`Arc<Closure>` -> `Arc<dyn Policy>`) ---
pub trait Policy: Send + Sync + 'static {}

impl<F, Fut> Policy for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
}

pub fn case_a_ok() -> Arc<dyn Policy> {
    Arc::new(|| async {
        let local = Rc::new(1i32);
        drop(local);
        yield_once().await;
    })
}

pub fn case_a_not_send() -> Arc<dyn Policy> {
    Arc::new(|| async {
        let local = Rc::new(1i32);
        yield_once().await;
        drop(local);
    })
}

// --- Case B: 2-level nested wrapper trait (`Wrapper<ServiceFn<Closure>>: Service`) ---
pub trait Service {
    fn run(&self);
}

pub struct ServiceFn<F>(pub F);

impl<F, Fut> Service for ServiceFn<F>
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send,
{
    fn run(&self) {}
}

pub struct Wrapper<S>(pub S);

impl<S> Service for Wrapper<S>
where
    S: Service,
{
    fn run(&self) {
        self.0.run();
    }
}

fn accept_service<S: Service>(s: S) {
    s.run();
}

pub fn case_b_ok() {
    let svc = Wrapper(ServiceFn(|| async {
        let local = Rc::new(1i32);
        drop(local);
        yield_once().await;
    }));
    accept_service(svc);
}

pub fn case_b_not_send() {
    let svc = Wrapper(ServiceFn(|| async {
        let local = Rc::new(1i32);
        yield_once().await;
        drop(local);
    }));
    accept_service(svc);
}

// --- Case C: 4-level projection through RPIT / `IntoFuture` ---
pub trait OrCancelExt: Sized {
    type Output;
    fn or_cancel(self) -> impl Future<Output = Result<Self::Output, ()>>;
}

impl<F: Future + Send> OrCancelExt for F {
    type Output = F::Output;
    fn or_cancel(self) -> impl Future<Output = Result<Self::Output, ()>> {
        async move { Ok(self.await) }
    }
}

pub fn or_cancel_fn<F: Future + Send>(f: F) -> impl Future<Output = Result<F::Output, ()>> {
    async move { Ok(f.await) }
}

pub async fn case_c_ext_ok() -> Result<i32, ()> {
    let res: Result<i32, ()> = async {
        let local = Rc::new(1i32);
        drop(local);
        yield_once().await;
        42
    }
    .or_cancel()
    .await;
    res
}

pub async fn case_c_ext_not_send() -> Result<i32, ()> {
    let res: Result<i32, ()> = async {
        let local = Rc::new(1i32);
        yield_once().await;
        drop(local);
        42
    }
    .or_cancel()
    .await;
    res
}

pub async fn case_c_fn_ok() -> Result<i32, ()> {
    let res: Result<i32, ()> = or_cancel_fn(async {
        let local = Rc::new(1i32);
        drop(local);
        yield_once().await;
        42
    })
    .await;
    res
}

pub async fn case_c_fn_not_send() -> Result<i32, ()> {
    let res: Result<i32, ()> = or_cancel_fn(async {
        let local = Rc::new(1i32);
        yield_once().await;
        drop(local);
        42
    })
    .await;
    res
}

fn main() {}
"#;

fn check_each(bodies: &[&str], see: &mut dyn FnMut(&str, &BodyVerdict<'_>)) {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/deep_interior/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "deep_interior",
        dir: "/deep_interior/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    for &name in bodies {
        let key = names
            .items(0)
            .find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key))
            .expect(name);
        let (v, borrows) = world.check_borrows(BodyRef::Free(key), alloc);
        println!("{name} (check_borrows): {v:?} (borrows: {borrows:?})");
        see(name, &v);
        let v_plain = world.check(BodyRef::Free(key), alloc);
        println!("{name} (check): {v_plain:?}");
        see(name, &v_plain);
    }
}

fn assert_clean(name: &str, v: &BodyVerdict<'_>) {
    assert!(
        v.is_clean() && v.reads.is_some(),
        "POSITIVE `{name}` must check clean on its exact interior: {v:?}"
    );
}

fn assert_not_send_error(name: &str, v: &BodyVerdict<'_>) {
    assert!(
        v.gaps.is_empty() && v.refused.is_none(),
        "NEGATIVE `{name}` must report a real type error, not a gap or refusal (e.g. ForeignWitness/INTERIOR_GAP): {v:?}"
    );
    assert!(
        !v.diagnostics.is_empty() && v.diagnostics.iter().all(|d| d.code == "E0277"),
        "NEGATIVE `{name}` must report E0277 (Rc<i32> held across .await), not E0282/E0283: {v:?}"
    );
}

#[test]
fn case_a_supertrait_unsizing_positive_and_negative() {
    check_each(&["case_a_ok"], &mut assert_clean);
    check_each(&["case_a_not_send"], &mut assert_not_send_error);
}

#[test]
fn case_b_nested_wrapper_trait_positive_and_negative() {
    check_each(&["case_b_ok"], &mut assert_clean);
    check_each(&["case_b_not_send"], &mut assert_not_send_error);
}

#[test]
fn case_c_rpit_projection_positive_and_negative() {
    check_each(&["case_c_ext_ok", "case_c_fn_ok"], &mut assert_clean);
    check_each(&["case_c_ext_not_send", "case_c_fn_not_send"], &mut assert_not_send_error);
}
