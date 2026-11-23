//! A closure that captures nothing, coerced to a fn pointer inside a constant (lane F4, wave
//! 2311): std's `thread_local!` writes `LocalKey::new(const { .. |_| { .. } .. })`, and core's
//! `OsFunctions::DEFAULT` holds such pointers. blaze-consteval's `ClosureFnPointer` arm makes the
//! pointer to the closure's `FnOnce::call_once` at `[C, (A..)]`, the item it asks of its world
//! (`ConstWorld::fn_trait_method`). front2's world (consts.rs `ConstsWorld`) answers it by the
//! one map the run's builder world reads (front2/langs.rs `fn_trait_method`); before, it gave the
//! trait's default `None` and every such constant was refused ("a closure as a fn pointer in a
//! world that names no `FnOnce::call_once`").
//!
//! The rule: the Reference, "Type coercions", "Coercion types": "Non capturing closures to `fn`
//! pointers"; "Closure types": "All closure types implement FnOnce". The lang traits mirror the
//! pinned library's (library/core/src/ops/function.rs), as tests/front2_foreign_closure.rs does.
//!
//! What it asserts: the const block in `pick` evaluates to one 8-byte word with one relocation,
//! to a fn (`DataTarget::Fn`). A world that does not answer `fn_trait_method` is refused.
//!
//! Line it prints (and asserts): `closure fn pointer: one word, one fn relocation`
//!
//! It needs no toolchain: the program is `#![no_core]`. NOT RUN by the writer (a write-only
//! lane).
#![feature(allocator_api)]

use blaze_build::data::DataTarget;
use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::const_at::block_key;
use blaze_ladder::front2::consts::{BodyRoot, Consts};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const PROGRAM: &str = r#"#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "copy"]
trait Copy {}

#[lang = "tuple_trait"]
trait Tuple {}

#[lang = "fn_once"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = "fn_once_output"]
    type Output;
    extern "rust-call" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = "fn_mut"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = "fn"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern "rust-call" fn call(&self, args: Args) -> Self::Output;
}

fn pick() -> fn(u8) -> u8 {
    const {
        let f: fn(u8) -> u8 = |x: u8| x;
        f
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    (pick())(7) as i32
}
"#;

#[test]
fn a_closure_in_a_constant_is_a_pointer_to_its_call_once() {
    let heap = blaze_own::testing::heap();
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let pick = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["pick"] }, heap.alloc())
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .unwrap_or_else(|| panic!("`pick` from the crate root"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(pick)).unwrap_or_else(|e| panic!("pick's head: {e:?}"));
    let owner = front.keys.item(pick).unwrap_or_else(|_| panic!("pick's checker key"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, pick).unwrap_or_else(|e| panic!("pick lowers: {e}"));
    let block = {
        let view = body.lowered.view();
        let at = (0..view.exprs.len() as u32)
            .find(|&i| matches!(view.expr(blaze_typeck::ExprId(i)), Ok(blaze_lower::Expr::Block { kind: blaze_lower::BlockKind::Const, .. })))
            .expect("`pick`'s const block");
        blaze_typeck::ExprId(at)
    };
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let mut consts = Consts::default();
    let key = block_key(owner, block, PROGRAM);
    let line = match consts.body_value(&mut records, prepared, owner, BodyRoot::Block(block), key, blaze_typeck::PTys::EMPTY, &[], None, heap.alloc()) {
        Ok(rec) => match (rec.data.allocs.len(), rec.data.allocs.first()) {
            (1, Some(root)) if root.bytes.len() == 8 && root.relocs.len() == 1 && matches!(root.relocs[0].target, DataTarget::Fn(_)) => {
                "closure fn pointer: one word, one fn relocation".to_string()
            }
            _ => format!("closure fn pointer: a record of another form: {rec:?}"),
        },
        Err(e) => format!("closure fn pointer: refused: {e}"),
    };
    println!("{line}");
    assert_eq!(line, "closure fn pointer: one word, one fn relocation");
}
