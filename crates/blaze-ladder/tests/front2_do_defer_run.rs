//! End-to-end real-Rust-source integration tests for lexical `defer { ... }` blocks and one-shot
//! postfix `.do` zero-`Pin` sans-IO machines through the full `blaze-ladder` pipeline
//! (`parser` -> `blaze-scope` -> `blaze-lower` -> `blaze-typeck` -> `blaze-build` ->
//! `blaze-lir::coroutine_machine` / in-process execution).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_build::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use blaze_build::{
    Abi, Adts, BuildError, DoMachineError, OverflowChecks, Panic, Subst, UbChecks, build_do_machine_body,
    has_do_exprs, split_do_machine_body, split_do_machine_body_bounded,
};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};
use blaze_ladder::seam::FrontEnd;
use blaze_lir::Op;
use blaze_lir::coroutine::Saved;
use blaze_lir::coroutine_machine::{
    MachineBudget, MachineEdge, MachineEdgeKind, MachineError, MachinePlant, split_machine_bounded,
};
use blaze_own::Alloc;
use bstd::machine::{
    EdgeKind, GraphEdge, GraphError, GraphPlant, HandleOp, MachineGraph, StateId, StateNode, verify_graph,
    verify_graph_with,
};
use blaze_surface::tool_attrs::{BlazeAttr, KNOWN, Stage, State, classify};
use blaze_ty::effect_ty::{
    EffectPolyError, EffectRow, EffectRowSet, EffectSet, EffectVar, LoweringMode, check_dyn_fn_effects,
    check_higher_order_call, specialize_call,
};
use blaze_ty::syscall_ty::{SyscallPolicyError, SyscallSet};
use blaze_typeck::{
    Adt, ExprId, ItemKey, Name, PTy, PortableTys, SansIoBodyFinding, TypeAttrs, TypedFinding,
    check_body_syscall_policy, check_sans_io_effects,
};

const EDITION: &str = "blaze-2026.11";

const NEW: NewFront<'static> = NewFront {
    package: "ladder",
    edition: EDITION,
    shelf: None,
    overflow: OverflowChecks::Off,
};

/// Real `#![no_core]` Rust source exercising:
/// 1. Normal block-exit LIFO `defer` ordering (`921`).
/// 2. Nested `defer` scopes and `defer` registered inside a `defer` body (`5_342_671`).
/// 3. Labeled `break 'b` running inner defers LIFO without firing outer defers early (`43_251`).
/// 4. Early `return` running defers LIFO while preserving the normal exit path (`99`/`721`, `42`/`821`).
/// 5. Contextual `defer` remaining an ordinary identifier (`fn defer`, `let defer = 10`).
const DEFER_PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}
impl Copy for i32 {}
impl Copy for bool {}

fn normal_lifo() -> u64 {
    let mut log: u64 = 0;
    {
        defer { log = log * 10 + 1; }
        defer { log = log * 10 + 2; }
        log = log * 10 + 9;
    }
    log
}

fn nested_defers() -> u64 {
    let mut log: u64 = 0;
    {
        defer {
            defer { log = log * 10 + 1; }
            log = log * 10 + 7;
        }
        {
            defer { log = log * 10 + 2; }
            {
                defer { log = log * 10 + 3; }
                log = 5;
            }
            log = log * 10 + 4;
        }
        log = log * 10 + 6;
    }
    log
}

fn labeled_break_defer() -> u64 {
    let mut log: u64 = 0;
    {
        defer { log = log * 10 + 1; }
        'b: {
            defer { log = log * 10 + 2; }
            defer { log = log * 10 + 3; }
            log = 4;
            break 'b;
        }
        log = log * 10 + 5;
    }
    log
}

fn helper_early(out: &mut u64, early: u64) -> u64 {
    defer { *out = *out * 10 + 1; }
    defer { *out = *out * 10 + 2; }
    if early == 1 {
        *out = *out * 10 + 7;
        return 99;
    }
    *out = *out * 10 + 8;
    42
}

fn defer(x: u64) -> u64 {
    let defer = 10u64;
    defer + x
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    if normal_lifo() != 921 {
        return 1;
    }
    if nested_defers() != 5_342_671 {
        return 2;
    }
    if labeled_break_defer() != 43_251 {
        return 3;
    }
    let mut early_log: u64 = 0;
    let r_early = helper_early(&mut early_log, 1);
    if r_early != 99 || early_log != 721 {
        return 4;
    }
    let mut normal_log: u64 = 0;
    let r_normal = helper_early(&mut normal_log, 0);
    if r_normal != 42 || normal_log != 821 {
        return 5;
    }
    if defer(5) != 15 {
        return 6;
    }
    77
}
"#;

/// Real `#![no_core]` Rust source with a two-suspend `.do` workflow and zero local borrows across
/// suspension points.
const DO_WORKFLOW_PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}

trait Request {
    type Answer;
}

struct Ping(u64);
impl Copy for Ping {}

impl Request for Ping {
    type Answer = u64;
}

fn workflow(x: u64, req: Ping) -> u64 {
    let y = x + 10;
    let a = req.do;
    let b = req.do;
    y + a + b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = workflow(1, Ping(2));
    0
}
"#;

/// Real `#![no_core]` Rust source that holds `&x` live across `req.do`, violating the zero-`Pin`
/// invariant and triggering `local-borrow-across-do` (`TypedFinding::LocalBorrowAcrossDo`).
const BAD_BORROW_DO_PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}

trait Request {
    type Answer;
}

struct Ping(u64);
impl Copy for Ping {}

impl Request for Ping {
    type Answer = u64;
}

fn bad(req: Ping) -> u64 {
    let x = 1u64;
    let r = &x;
    let a = req.do;
    a + *r
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = bad(Ping(1));
    0
}
"#;

/// Thin `blaze_build::Adts` adapter over a resolved [`Front2Program`] for feeding real lowered +
/// typechecked Rust bodies directly into [`build_do_machine_body`] / [`split_do_machine_body`].
struct ProgramAdts<'a, 'n>(&'a Front2Program<'n>);

impl LayoutFacts for ProgramAdts<'_, '_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.0.adt(key)
    }

    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
        self.0.repr(key)
    }

    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        self.0.discriminant(key, variant)
    }

    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> {
        self.0.valid_range(key)
    }

    fn normalize(
        &self,
        trait_: ItemKey,
        name: Name,
        args: &[PTy],
        ground: &mut PortableTys<'_>,
    ) -> Result<PTy, BuildError> {
        match self.0.normalize(trait_, name, args, ground) {
            Ok(Some(t)) => Ok(t),
            Ok(None) => Err(BuildError::Projection { trait_, name }),
            Err(e) => Err(BuildError::Select(e)),
        }
    }
}

impl Adts for ProgramAdts<'_, '_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.0.adt(key)
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn overflow_checks(&self) -> OverflowChecks {
        OverflowChecks::Off
    }

    fn ub_checks(&self) -> UbChecks {
        UbChecks::Off
    }

    fn panic(&self) -> Panic {
        Panic::Abort
    }

    fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        self.0.int_lit(owner, expr)
    }

    fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        Ok(None)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        self.0.fn_abi(item)
    }
}

#[test]
fn real_rust_defer_executes_lifo_on_normal_break_and_early_return() {
    let files = [SourceFile { path: "src/main.rs", text: DEFER_PROGRAM }];
    let got = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .map_err(|e| format!("{e}").replace('\n', " "));
    assert_eq!(got, Ok(77), "real Rust defer program must execute all LIFO traces and return 77");
}

#[test]
fn real_rust_postfix_do_parses_lowers_typechecks_and_splits_zero_pin_machine() {
    let alloc = Alloc::global();
    let tc = Toolchain::none(alloc);
    let root = "/do-demo/src/main.rs";
    let files = [(root.to_owned(), DO_WORKFLOW_PROGRAM.to_owned())];
    let user = UserCrate {
        name: "do_demo",
        dir: "/do-demo/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest(EDITION).expect("blaze edition"),
        panic: Panic::Abort,
    };
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
    let sources = [SourceFile { path: "src/main.rs", text: DO_WORKFLOW_PROGRAM }];

    // 1. Check via `ProjectWorld`: zero diagnostics and zero `local-borrow-across-do` findings.
    let check_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("check frame");
    let mut world = ProjectWorld::new(&names, &check_frame, &sources, alloc).expect("project world");
    let wf_lib_key = names
        .items(0)
        .find_map(|(k, _, _)| (names.item_text(k)?.name == Some("workflow")).then_some(k))
        .expect("workflow item");
    let verdict = world.check(BodyRef::Free(wf_lib_key), alloc);
    assert!(verdict.is_clean(), "workflow must typecheck clean: {verdict:?}");
    assert!(verdict.undecided.is_empty(), "workflow must have no undecided items: {verdict:?}");
    assert!(verdict.findings.is_empty(), "workflow must have zero typed lint findings: {verdict:?}");

    // 2. Build via `Front2Program` and verify `checked.suspends == true` and zero findings.
    let prog_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("program frame");
    let mut program = Front2Program::new(&names, &prog_frame, &sources, alloc).expect("front2 program");
    let checked = program.check_all().expect("check_all");
    let wf_idx = (0..program.fn_count())
        .find(|&i| {
            let path = program.function(i).path;
            path == "workflow" || path.ends_with("::workflow")
        })
        .expect("workflow row in Front2Program");
    let wf_checked = checked[wf_idx].as_ref().expect("workflow checked record");
    assert!(wf_checked.suspends, "`.do` workflow must mark checked.suspends == true");
    assert!(wf_checked.diags.is_empty(), "workflow diagnostics must be empty: {:?}", wf_checked.diags);
    assert!(
        wf_checked.findings.is_empty(),
        "workflow must have zero local-borrow-across-do findings: {:?}",
        wf_checked.findings,
    );

    let (body, _) = program.function(wf_idx).body.expect("workflow body");
    let view = body.view(program.body_types(wf_idx));
    let sig = program.sig(wf_idx);
    let adts = ProgramAdts(&program);

    // 3. Lower real parsed/lowered/typechecked body to pre-split LIR with 2 `Op::Suspend`s.
    let mut ground = PortableTys::new_in(alloc);
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: alloc };
    let (pre_func, shape) = build_do_machine_body(&view, wf_checked, &sig, &mut subst, &adts, 2, Global)
        .unwrap_or_else(|e| panic!("build_do_machine_body failed: {e}"));
    blaze_lir::verify(pre_func.view()).expect("pre-split `.do` body must verify");
    let suspend_count = (0..pre_func.view().len()).filter(|&i| pre_func.view().op(i) == Op::Suspend).count();
    assert_eq!(suspend_count, 2, "expected 2 Op::Suspend ops in pre-split LIR");
    assert_eq!(shape.state_param, 0);
    assert_eq!(shape.grace_max_steps, 2);

    // 4. Split via `split_do_machine_body`: prove zero-`Pin` and exact state transition graph.
    let mut ground2 = PortableTys::new_in(alloc);
    let mut subst2 = Subst { args: &[], ground: &mut ground2, scratch: alloc };
    let split = split_do_machine_body(&view, wf_checked, &sig, &mut subst2, &adts, 2, Global)
        .unwrap_or_else(|e| panic!("split_do_machine_body failed: {e}"));
    assert!(split.graph.zero_pin_proven, "`.do` machine must prove zero_pin_proven");
    assert_eq!(split.graph.points, 2);
    assert!(
        split.layout.fields.iter().all(|f| matches!(f.what, Saved::Value(_))),
        "zero-Pin machine must not pin stack slots in state: {:?}",
        split.layout.fields,
    );
    blaze_lir::verify(split.step.view()).expect("split step function must verify");
    blaze_lir::verify(split.on_cancel.view()).expect("split on_cancel function must verify");
    let expected_edges = [
        MachineEdge { from_point: None, to_point: None, kind: MachineEdgeKind::Cancel },
        MachineEdge { from_point: None, to_point: Some(0), kind: MachineEdgeKind::Suspend },
        MachineEdge { from_point: Some(0), to_point: None, kind: MachineEdgeKind::Cancel },
        MachineEdge { from_point: Some(0), to_point: Some(1), kind: MachineEdgeKind::Suspend },
        MachineEdge { from_point: Some(1), to_point: None, kind: MachineEdgeKind::Return },
        MachineEdge { from_point: Some(1), to_point: None, kind: MachineEdgeKind::Cancel },
    ];
    assert_eq!(split.graph.edges.as_slice(), &expected_edges);

    // 5. Enforce `#[blaze::max_state_bytes]` ceiling via `split_machine_bounded`.
    let state_size = split.layout.size;
    assert!(state_size > 0, "suspended workflow state must have non-zero size");
    let exact = split_machine_bounded(
        pre_func.view(),
        shape,
        MachineBudget::max_bytes(state_size),
        MachinePlant::None,
        Global,
    )
    .expect("exact state_size budget must pass");
    assert_eq!(exact.layout.size, state_size);
    assert!(exact.graph.zero_pin_proven);

    let tight = state_size - 1;
    let err = split_machine_bounded(
        pre_func.view(),
        shape,
        MachineBudget::max_bytes(tight),
        MachinePlant::None,
        Global,
    )
    .err()
    .expect("budget smaller than actual state size must fail with StateSizeExceeded");
    match err {
        MachineError::StateSizeExceeded { actual, max, largest_field_size } => {
            assert_eq!(actual, state_size);
            assert_eq!(max, tight);
            assert!(largest_field_size > 0, "largest_field_size must be positive");
        }
        other => panic!("expected MachineError::StateSizeExceeded, got {other:?}"),
    }
}

#[test]
fn real_rust_local_borrow_across_do_is_rejected_by_typeck() {
    let alloc = Alloc::global();
    let tc = Toolchain::none(alloc);
    let root = "/do-bad/src/main.rs";
    let files = [(root.to_owned(), BAD_BORROW_DO_PROGRAM.to_owned())];
    let user = UserCrate {
        name: "do_bad",
        dir: "/do-bad/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest(EDITION).expect("blaze edition"),
        panic: Panic::Abort,
    };
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
    let sources = [SourceFile { path: "src/main.rs", text: BAD_BORROW_DO_PROGRAM }];

    // 1. Check via `ProjectWorld`: `local-borrow-across-do` finding is reported.
    let check_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("check frame");
    let mut world = ProjectWorld::new(&names, &check_frame, &sources, alloc).expect("project world");
    let bad_lib_key = names
        .items(0)
        .find_map(|(k, _, _)| (names.item_text(k)?.name == Some("bad")).then_some(k))
        .expect("bad item");
    let verdict = world.check(BodyRef::Free(bad_lib_key), alloc);
    assert!(verdict.is_clean(), "bad should finish typecheck with a typed lint finding: {verdict:?}");
    assert_eq!(verdict.findings.len(), 1, "expected 1 local-borrow-across-do finding: {:?}", verdict.findings);
    assert_eq!(verdict.findings[0].lint, "local-borrow-across-do");
    assert!(
        verdict.findings[0].text.contains("local variable is borrowed across a `.do` suspension point"),
        "unexpected finding text: {}",
        verdict.findings[0].text,
    );

    // 2. Also verify the underlying `TypedFinding::LocalBorrowAcrossDo` record on `Checked`.
    let prog_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("program frame");
    let mut program = Front2Program::new(&names, &prog_frame, &sources, alloc).expect("front2 program");
    let checked = program.check_all().expect("check_all");
    let bad_idx = (0..program.fn_count())
        .find(|&i| {
            let path = program.function(i).path;
            path == "bad" || path.ends_with("::bad")
        })
        .expect("bad row in Front2Program");
    let bad_checked = checked[bad_idx].as_ref().expect("bad checked record");
    assert!(bad_checked.suspends, "bad must mark checked.suspends == true");
    assert_eq!(bad_checked.findings.len(), 1, "expected 1 TypedFinding: {:?}", bad_checked.findings);
    assert!(
        matches!(bad_checked.findings[0], TypedFinding::LocalBorrowAcrossDo { local, .. } if local == Name::of("x")),
        "expected TypedFinding::LocalBorrowAcrossDo for local `x`, got {:?}",
        bad_checked.findings[0],
    );
}

/// Real `#![no_core]` Rust source with a pure helper fn and a `.do` workflow annotated with
/// `#[blaze::max_state_bytes(16)]`.
const MAX_STATE_BYTES_PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}

trait Request {
    type Answer;
}

struct Ping(u64);
impl Copy for Ping {}

impl Request for Ping {
    type Answer = u64;
}

fn pure_add(x: u64, y: u64) -> u64 {
    x + y
}

#[blaze::max_state_bytes(16)]
fn bounded_workflow(x: u64, req: Ping) -> u64 {
    let y = pure_add(x, 10);
    let a = req.do;
    y + a
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = bounded_workflow(1, Ping(2));
    0
}
"#;

/// Real `#![no_core]` Rust source with 2 nested `defer` blocks wrapping 2 `.do` suspension points,
/// proving LIFO cleanup paths on both normal exit and cancellation with zero local borrows across
/// `.do`.
const DEFER_DO_PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}

trait Request {
    type Answer;
}

struct Ping(u64);
impl Copy for Ping {}

impl Request for Ping {
    type Answer = u64;
}

fn deferred_workflow(out: &mut u64, req: Ping) -> u64 {
    {
        defer { *out = *out * 10 + 1; }
        let a = req.do;
        {
            defer { *out = *out * 10 + 2; }
            let b = req.do;
            a + b
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut log: u64 = 0;
    let _ = deferred_workflow(&mut log, Ping(1));
    0
}
"#;

/// Counts constant-value `Op::Store` instructions targeting `tag_offset` in `view`.
fn count_const_tag_stores(view: blaze_lir::View<'_>, tag_offset: u32) -> usize {
    let mut count = 0usize;
    for inst in 0..view.len() {
        if view.op(inst) != Op::Store {
            continue;
        }
        let mut o = view.operands(inst);
        let value = o.value();
        let _ptr = o.value();
        let offset = o.offset();
        if offset == tag_offset as i32 && view.op(value.index()) == Op::Iconst {
            count += 1;
        }
    }
    count
}

#[test]
fn real_rust_do_machine_respects_max_state_bytes_attr_and_bounded_split() {
    let BlazeAttr::Known(idx) = classify("max_state_bytes") else {
        panic!("expected `blaze::max_state_bytes` to classify as Known");
    };
    assert_eq!(KNOWN[usize::from(idx)].state, State::Checked(Stage::Effects));

    let alloc = Alloc::global();
    let tc = Toolchain::none(alloc);
    let root = "/do-budget/src/main.rs";
    let files = [(root.to_owned(), MAX_STATE_BYTES_PROGRAM.to_owned())];
    let user = UserCrate {
        name: "do_budget",
        dir: "/do-budget/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest(EDITION).expect("blaze edition"),
        panic: Panic::Abort,
    };
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
    let sources = [SourceFile { path: "src/main.rs", text: MAX_STATE_BYTES_PROGRAM }];

    let check_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("check frame");
    let mut world = ProjectWorld::new(&names, &check_frame, &sources, alloc).expect("project world");
    for fn_name in ["pure_add", "bounded_workflow"] {
        let key = names
            .items(0)
            .find_map(|(k, _, _)| (names.item_text(k)?.name == Some(fn_name)).then_some(k))
            .unwrap_or_else(|| panic!("missing item `{fn_name}`"));
        let verdict = world.check(BodyRef::Free(key), alloc);
        assert!(verdict.is_clean(), "`{fn_name}` must typecheck clean: {verdict:?}");
        assert!(verdict.undecided.is_empty(), "`{fn_name}` must have no undecided items: {verdict:?}");
        assert!(verdict.findings.is_empty(), "`{fn_name}` must have zero findings: {verdict:?}");
    }

    let prog_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("program frame");
    let mut program = Front2Program::new(&names, &prog_frame, &sources, alloc).expect("front2 program");
    let checked = program.check_all().expect("check_all");

    let find_fn = |name: &str| {
        (0..program.fn_count())
            .find(|&i| {
                let path = program.function(i).path;
                path == name || path.ends_with(&format!("::{name}"))
            })
            .unwrap_or_else(|| panic!("missing `{name}` in Front2Program"))
    };

    let pure_idx = find_fn("pure_add");
    let pure_checked = checked[pure_idx].as_ref().expect("pure_add checked record");
    let (pure_body, _) = program.function(pure_idx).body.expect("pure_add body");
    let pure_view = pure_body.view(program.body_types(pure_idx));
    assert!(!pure_checked.suspends, "pure fn must not mark checked.suspends");
    assert!(!has_do_exprs(&pure_view), "pure fn must report has_do_exprs == false");

    let wf_idx = find_fn("bounded_workflow");
    let wf_checked = checked[wf_idx].as_ref().expect("bounded_workflow checked record");
    let (wf_body, _) = program.function(wf_idx).body.expect("bounded_workflow body");
    let wf_view = wf_body.view(program.body_types(wf_idx));
    let wf_sig = program.sig(wf_idx);
    let adts = ProgramAdts(&program);
    assert!(wf_checked.suspends, "`.do` workflow must mark checked.suspends == true");
    assert!(has_do_exprs(&wf_view), "`.do` workflow must report has_do_exprs == true");

    let mut ground_unbounded = PortableTys::new_in(alloc);
    let mut subst_unbounded = Subst { args: &[], ground: &mut ground_unbounded, scratch: alloc };
    let unbounded = split_do_machine_body_bounded(
        &wf_view,
        wf_checked,
        &wf_sig,
        &mut subst_unbounded,
        &adts,
        1,
        MachineBudget::UNBOUNDED,
        Global,
    )
    .unwrap_or_else(|e| panic!("unbounded split_do_machine_body_bounded failed: {e}"));
    let state_size = unbounded.layout.size;
    assert!(state_size > 0, "suspended state must have non-zero size");
    assert!(unbounded.graph.zero_pin_proven);
    assert_eq!(unbounded.graph.points, 1);

    let mut ground_exact = PortableTys::new_in(alloc);
    let mut subst_exact = Subst { args: &[], ground: &mut ground_exact, scratch: alloc };
    let exact = split_do_machine_body_bounded(
        &wf_view,
        wf_checked,
        &wf_sig,
        &mut subst_exact,
        &adts,
        1,
        MachineBudget::max_bytes(state_size),
        Global,
    )
    .unwrap_or_else(|e| panic!("exact state_size budget must pass: {e}"));
    assert_eq!(exact.layout.size, state_size);
    assert!(exact.graph.zero_pin_proven);

    let tight = state_size - 1;
    let mut ground_tight = PortableTys::new_in(alloc);
    let mut subst_tight = Subst { args: &[], ground: &mut ground_tight, scratch: alloc };
    let err = split_do_machine_body_bounded(
        &wf_view,
        wf_checked,
        &wf_sig,
        &mut subst_tight,
        &adts,
        1,
        MachineBudget::max_bytes(tight),
        Global,
    )
    .err()
    .expect("budget - 1 must fail with DoMachineError::Machine(MachineError::StateSizeExceeded)");
    match err {
        DoMachineError::Machine(MachineError::StateSizeExceeded { actual, max, largest_field_size }) => {
            assert_eq!(actual, state_size);
            assert_eq!(max, tight);
            assert!(largest_field_size > 0, "largest_field_size must be positive");
        }
        other => panic!("expected DoMachineError::Machine(StateSizeExceeded), got {other:?}"),
    }
}

#[test]
fn real_rust_defer_lifo_on_cancel_and_normal_exit_graph_proof() {
    let alloc = Alloc::global();
    let tc = Toolchain::none(alloc);
    let root = "/defer-do/src/main.rs";
    let files = [(root.to_owned(), DEFER_DO_PROGRAM.to_owned())];
    let user = UserCrate {
        name: "defer_do",
        dir: "/defer-do/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest(EDITION).expect("blaze edition"),
        panic: Panic::Abort,
    };
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
    let sources = [SourceFile { path: "src/main.rs", text: DEFER_DO_PROGRAM }];

    let check_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("check frame");
    let mut world = ProjectWorld::new(&names, &check_frame, &sources, alloc).expect("project world");
    let wf_key = names
        .items(0)
        .find_map(|(k, _, _)| (names.item_text(k)?.name == Some("deferred_workflow")).then_some(k))
        .expect("deferred_workflow item");
    let verdict = world.check(BodyRef::Free(wf_key), alloc);
    assert!(verdict.is_clean(), "deferred_workflow must typecheck clean: {verdict:?}");
    assert!(verdict.undecided.is_empty(), "deferred_workflow must have no undecided items: {verdict:?}");
    assert!(verdict.findings.is_empty(), "deferred_workflow must have zero findings: {verdict:?}");

    let prog_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("program frame");
    let mut program = Front2Program::new(&names, &prog_frame, &sources, alloc).expect("front2 program");
    let checked = program.check_all().expect("check_all");
    let wf_idx = (0..program.fn_count())
        .find(|&i| {
            let path = program.function(i).path;
            path == "deferred_workflow" || path.ends_with("::deferred_workflow")
        })
        .expect("deferred_workflow row in Front2Program");
    let wf_checked = checked[wf_idx].as_ref().expect("deferred_workflow checked record");
    assert!(wf_checked.suspends, "deferred_workflow must mark checked.suspends == true");

    let (body, _) = program.function(wf_idx).body.expect("deferred_workflow body");
    let view = body.view(program.body_types(wf_idx));
    assert!(has_do_exprs(&view), "deferred_workflow must report has_do_exprs == true");
    let sig = program.sig(wf_idx);
    let adts = ProgramAdts(&program);

    // 1. Build pre-split LIR and confirm 2 suspend points and bounded grace_max_steps == 2.
    let mut ground_pre = PortableTys::new_in(alloc);
    let mut subst_pre = Subst { args: &[], ground: &mut ground_pre, scratch: alloc };
    let (pre_func, shape) = build_do_machine_body(&view, wf_checked, &sig, &mut subst_pre, &adts, 2, Global)
        .unwrap_or_else(|e| panic!("build_do_machine_body failed: {e}"));
    blaze_lir::verify(pre_func.view()).expect("pre-split deferred `.do` body must verify");
    let suspend_count = (0..pre_func.view().len()).filter(|&i| pre_func.view().op(i) == Op::Suspend).count();
    assert_eq!(suspend_count, 2, "expected 2 Op::Suspend ops in nested defer `.do` workflow");
    assert_eq!(shape.grace_max_steps, 2);

    // 2. Lower via `split_do_machine_body_bounded` and verify `on_cancel` reachability from every
    //    suspended state plus normal exit path.
    let mut ground_split = PortableTys::new_in(alloc);
    let mut subst_split = Subst { args: &[], ground: &mut ground_split, scratch: alloc };
    let split = split_do_machine_body_bounded(
        &view,
        wf_checked,
        &sig,
        &mut subst_split,
        &adts,
        2,
        MachineBudget::UNBOUNDED,
        Global,
    )
    .unwrap_or_else(|e| panic!("split_do_machine_body_bounded failed: {e}"));
    assert!(split.graph.zero_pin_proven, "nested defer `.do` machine must prove zero_pin_proven");
    assert_eq!(split.graph.points, 2);
    assert!(split.graph.reaches_return(), "machine must reach normal Return");
    assert!(split.graph.has_cancel_edge(0), "suspend point 0 must reach on_cancel");
    assert!(split.graph.has_cancel_edge(1), "suspend point 1 must reach on_cancel");
    assert!(
        split
            .graph
            .edges
            .contains(&MachineEdge { from_point: None, to_point: None, kind: MachineEdgeKind::Cancel }),
        "entry state must have an unresumed Cancel edge: {:?}",
        split.graph.edges,
    );
    blaze_lir::verify(split.step.view()).expect("split step function must verify");
    blaze_lir::verify(split.on_cancel.view()).expect("split on_cancel function must verify");

    // 3. Project the lowered transition summary + LIFO defer cleanup chain into `MachineGraph`
    //    and prove bounded cancellation termination within `shape.grace_max_steps` (2 steps:
    //    inner defer -> outer defer -> Abandon), plus planted-bug controls.
    let states = [
        StateNode { id: StateId(0), name: "Entry", loop_state: false, initial_handles: 0 },
        StateNode { id: StateId(1), name: "Suspend0", loop_state: false, initial_handles: 0 },
        StateNode { id: StateId(2), name: "Suspend1", loop_state: false, initial_handles: 0 },
        StateNode { id: StateId(3), name: "CancelInnerDefer", loop_state: false, initial_handles: 0 },
        StateNode { id: StateId(4), name: "CancelOuterDefer", loop_state: false, initial_handles: 0 },
    ];
    let mut graph_edges = std::vec::Vec::new();
    for edge in &split.graph.edges {
        let from = match edge.from_point {
            None => StateId(0),
            Some(p) => StateId((p + 1) as u16),
        };
        let (kind, label) = match (edge.kind, edge.from_point, edge.to_point) {
            (MachineEdgeKind::Suspend, _, Some(to)) => (EdgeKind::Ask { to: StateId((to + 1) as u16) }, "do_suspend"),
            (MachineEdgeKind::Return, _, None) => (EdgeKind::Done, "normal_exit_lifo_defers"),
            (MachineEdgeKind::Cancel, None, None) => (EdgeKind::Abandon, "cancel_unresumed"),
            (MachineEdgeKind::Cancel, Some(0), None) => {
                (EdgeKind::CancelCleanup { to: StateId(4) }, "cancel_p0_outer_defer")
            }
            (MachineEdgeKind::Cancel, Some(1), None) => {
                (EdgeKind::CancelCleanup { to: StateId(3) }, "cancel_p1_inner_defer")
            }
            other => panic!("unexpected lowered machine edge: {other:?}"),
        };
        graph_edges.push(GraphEdge { from, kind, handle_op: HandleOp::None, label });
    }
    graph_edges.push(GraphEdge {
        from: StateId(3),
        kind: EdgeKind::CancelCleanup { to: StateId(4) },
        handle_op: HandleOp::None,
        label: "lifo_inner_to_outer_defer",
    });
    graph_edges.push(GraphEdge {
        from: StateId(4),
        kind: EdgeKind::Abandon,
        handle_op: HandleOp::None,
        label: "outer_defer_complete",
    });

    let verified_graph = MachineGraph {
        name: "deferred_workflow",
        entry: StateId(0),
        grace_max_steps: shape.grace_max_steps,
        states: &states,
        edges: &graph_edges,
    };
    assert_eq!(verify_graph(&verified_graph), Ok(()));

    // Tighter grace budget (1 < 2 nested defer cleanup steps) must be rejected by `verify_graph`,
    // and only passes when `GraphPlant::SkipCancelBoundCheck` is planted.
    let tight_grace_graph = MachineGraph { grace_max_steps: 1, ..verified_graph };
    assert_eq!(
        verify_graph(&tight_grace_graph),
        Err(GraphError::UnboundedCancelCleanup { start: StateId(2), steps: 2, max_steps: 1 }),
    );
    assert_eq!(verify_graph_with(&tight_grace_graph, GraphPlant::SkipCancelBoundCheck), Ok(()));

    // LIR-level `MachinePlant` controls on the exact real-Rust pre-split body:
    // (a) `MachinePlant::SkipStateSizeCheck` bypasses a tight budget that `MachinePlant::None` rejects.
    let tight_budget = MachineBudget::max_bytes(split.layout.size - 1);
    assert!(matches!(
        split_machine_bounded(pre_func.view(), shape, tight_budget, MachinePlant::None, Global),
        Err(MachineError::StateSizeExceeded { .. }),
    ));
    let bypassed = split_machine_bounded(
        pre_func.view(),
        shape,
        tight_budget,
        MachinePlant::SkipStateSizeCheck,
        Global,
    )
    .expect("SkipStateSizeCheck plant must bypass tight budget");
    assert_eq!(bypassed.layout.size, split.layout.size);

    // (b) Both suspend points plus terminal return emit explicit tag stores in `step`, and both
    //     lowered functions (`step` and `on_cancel`) verify cleanly as LIR.
    let clean_tag_stores = count_const_tag_stores(split.step.view(), split.layout.tag_offset);
    assert!(
        clean_tag_stores >= 3,
        "two suspend points + terminal return must emit >= 3 tag stores (got {clean_tag_stores})",
    );
    blaze_lir::verify(split.step.view()).expect("split.step must verify cleanly");
    blaze_lir::verify(split.on_cancel.view()).expect("split.on_cancel must verify cleanly");
}

/// Real `#![no_core]` Rust source containing a pure function and a suspending `.do` function used
/// to verify effect-polymorphic specialization (`PlainFn` vs `MachineCoroutine`) end to end.
const EFFECT_POLY_PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}

trait Request {
    type Answer;
}

struct Ping(u64);
impl Copy for Ping {}

impl Request for Ping {
    type Answer = u64;
}

fn pure_step(x: u64) -> u64 {
    x + 1
}

fn suspending_step(x: u64, req: Ping) -> u64 {
    let a = req.do;
    x + a
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let p = pure_step(1);
    let _ = suspending_step(p, Ping(2));
    0
}
"#;

/// Real `#![no_core]` Rust source carrying both `#![blaze::sans_io]` at the crate root and
/// `#[blaze::syscalls(fs, clock)]` on a driver-boundary function.
const SANS_IO_SYSCALLS_PROGRAM: &str = r#"#![feature(no_core, lang_items, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}

fn pure_core(x: u64) -> u64 {
    x + 1
}

#[blaze::syscalls(fs, clock)]
fn fs_clock_driver(x: u64) -> u64 {
    pure_core(x) + 2
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = fs_clock_driver(1);
    0
}
"#;

#[test]
fn real_rust_effect_polymorphic_higher_order_lowers_pure_to_plain_fn_and_do_to_machine() {
    let alloc = Alloc::global();
    let tc = Toolchain::none(alloc);
    let root = "/effect-poly/src/main.rs";
    let files = [(root.to_owned(), EFFECT_POLY_PROGRAM.to_owned())];
    let user = UserCrate {
        name: "effect_poly",
        dir: "/effect-poly/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest(EDITION).expect("blaze edition"),
        panic: Panic::Abort,
    };
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
    let sources = [SourceFile { path: "src/main.rs", text: EFFECT_POLY_PROGRAM }];

    let prog_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("program frame");
    let mut program = Front2Program::new(&names, &prog_frame, &sources, alloc).expect("front2 program");
    let checked = program.check_all().expect("check_all");

    let find_fn = |name: &str| {
        (0..program.fn_count())
            .find(|&i| {
                let path = program.function(i).path;
                path == name || path.ends_with(&format!("::{name}"))
            })
            .unwrap_or_else(|| panic!("missing `{name}` in Front2Program"))
    };

    // 1. Real pure body: zero `.do` expressions -> `EffectSet::PURE` -> `LoweringMode::PlainFn`.
    let pure_idx = find_fn("pure_step");
    let pure_checked = checked[pure_idx].as_ref().expect("pure_step checked record");
    assert!(pure_checked.diags.is_empty(), "pure_step diags: {:?}", pure_checked.diags);
    assert!(!pure_checked.suspends, "pure_step must not mark checked.suspends");
    let (pure_body, _) = program.function(pure_idx).body.expect("pure_step body");
    let pure_view = pure_body.view(program.body_types(pure_idx));
    assert!(!has_do_exprs(&pure_view), "pure_step must report has_do_exprs == false");

    // 2. Real `.do` body: has `.do` expression -> `EffectSet::SUSPEND` -> `LoweringMode::MachineCoroutine`.
    let do_idx = find_fn("suspending_step");
    let do_checked = checked[do_idx].as_ref().expect("suspending_step checked record");
    assert!(do_checked.diags.is_empty(), "suspending_step diags: {:?}", do_checked.diags);
    assert!(do_checked.suspends, "suspending_step must mark checked.suspends == true");
    let (do_body, _) = program.function(do_idx).body.expect("suspending_step body");
    let do_view = do_body.view(program.body_types(do_idx));
    assert!(has_do_exprs(&do_view), "suspending_step must report has_do_exprs == true");
    let do_sig = program.sig(do_idx);
    let adts = ProgramAdts(&program);

    let mut ground = PortableTys::new_in(alloc);
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: alloc };
    let split = split_do_machine_body_bounded(
        &do_view,
        do_checked,
        &do_sig,
        &mut subst,
        &adts,
        1,
        MachineBudget::UNBOUNDED,
        Global,
    )
    .unwrap_or_else(|e| panic!("split_do_machine_body_bounded failed: {e}"));
    assert!(split.graph.zero_pin_proven);
    assert!(split.graph.points >= 1, "expected >= 1 suspend point, got {}", split.graph.points);

    // 3. Higher-order polymorphic effect row `expect(e)` specializes cleanly to `PlainFn` for the
    //    pure callback and `MachineCoroutine` for the `.do` callback.
    let e = EffectVar(0);
    let ho_row_set = EffectRowSet::poly(EffectSet::PURE, e);
    let ho_row = EffectRow::poly(EffectSet::PURE, e);
    assert_eq!(ho_row_set.lowering_mode(), None, "uninstantiated polymorphic row has no fixed mode");

    let pure_effects = if has_do_exprs(&pure_view) { EffectSet::SUSPEND } else { EffectSet::PURE };
    let do_effects = if has_do_exprs(&do_view) { EffectSet::SUSPEND } else { EffectSet::PURE };
    assert_eq!(pure_effects, EffectSet::PURE);
    assert_eq!(do_effects, EffectSet::SUSPEND);
    assert_eq!(LoweringMode::for_effects(pure_effects), LoweringMode::PlainFn);
    assert_eq!(LoweringMode::for_effects(do_effects), LoweringMode::MachineCoroutine);

    assert_eq!(
        specialize_call(ho_row_set, &[(e, pure_effects)], EffectSet::PURE),
        Ok((EffectSet::PURE, LoweringMode::PlainFn)),
    );
    assert_eq!(
        check_higher_order_call(ho_row, pure_effects, EffectSet::PURE),
        Ok((EffectSet::PURE, LoweringMode::PlainFn)),
    );

    assert_eq!(
        specialize_call(ho_row_set, &[(e, do_effects)], EffectSet::SUSPEND),
        Ok((EffectSet::SUSPEND, LoweringMode::MachineCoroutine)),
    );
    assert_eq!(
        check_higher_order_call(ho_row, do_effects, EffectSet::SUSPEND),
        Ok((EffectSet::SUSPEND, LoweringMode::MachineCoroutine)),
    );
    assert_eq!(
        specialize_call(ho_row_set, &[(e, do_effects)], EffectSet::PURE),
        Err(EffectPolyError::EffectExceedsAllowed {
            required: EffectSet::SUSPEND,
            allowed: EffectSet::PURE,
        }),
    );

    // 4. `dyn Fn` object-safety: pure callback coerces cleanly, while suspending `.do` callback
    //    is refused with `EffectPolyError::SuspendBehindDyn`.
    assert_eq!(check_dyn_fn_effects(pure_effects, EffectSet::ALLOC), Ok(()));
    assert_eq!(
        check_dyn_fn_effects(do_effects, EffectSet::ALL),
        Err(EffectPolyError::SuspendBehindDyn { callback_effects: EffectSet::SUSPEND }),
    );
}

#[test]
fn real_rust_sans_io_and_syscalls_attr_policy_enforcement() {
    // 1. Verify `blaze-surface` classifies both `sans_io` and `syscalls` in `Stage::Effects`.
    let BlazeAttr::Known(sans_io_idx) = classify("sans_io") else {
        panic!("expected `blaze::sans_io` to classify as Known");
    };
    assert_eq!(KNOWN[usize::from(sans_io_idx)].planned, Stage::Effects);

    let BlazeAttr::Known(syscalls_idx) = classify("syscalls") else {
        panic!("expected `blaze::syscalls` to classify as Known");
    };
    assert_eq!(KNOWN[usize::from(syscalls_idx)].state, State::Checked(Stage::Effects));

    // 2. Compile real `#![no_core]` Rust source carrying `#![blaze::sans_io]` and
    //    `#[blaze::syscalls(fs, clock)]` cleanly through `front2` with zero diagnostics.
    let alloc = Alloc::global();
    let tc = Toolchain::none(alloc);
    let root = "/sans-io-policy/src/main.rs";
    let files = [(root.to_owned(), SANS_IO_SYSCALLS_PROGRAM.to_owned())];
    let user = UserCrate {
        name: "sans_io_policy",
        dir: "/sans-io-policy/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest(EDITION).expect("blaze edition"),
        panic: Panic::Abort,
    };
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
    let sources = [SourceFile { path: "src/main.rs", text: SANS_IO_SYSCALLS_PROGRAM }];

    let check_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("check frame");
    let mut world = ProjectWorld::new(&names, &check_frame, &sources, alloc).expect("project world");
    for fn_name in ["pure_core", "fs_clock_driver"] {
        let key = names
            .items(0)
            .find_map(|(k, _, _)| (names.item_text(k)?.name == Some(fn_name)).then_some(k))
            .unwrap_or_else(|| panic!("missing item `{fn_name}`"));
        let verdict = world.check(BodyRef::Free(key), alloc);
        assert!(verdict.is_clean(), "`{fn_name}` must typecheck clean: {verdict:?}");
        assert!(verdict.undecided.is_empty(), "`{fn_name}` must have no undecided items: {verdict:?}");
        assert!(verdict.findings.is_empty(), "`{fn_name}` must have zero findings: {verdict:?}");
    }

    let prog_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("program frame");
    let mut program = Front2Program::new(&names, &prog_frame, &sources, alloc).expect("front2 program");
    let checked = program.check_all().expect("check_all");

    let find_fn = |name: &str| {
        (0..program.fn_count())
            .find(|&i| {
                let path = program.function(i).path;
                path == name || path.ends_with(&format!("::{name}"))
            })
            .unwrap_or_else(|| panic!("missing `{name}` in Front2Program"))
    };

    let pure_idx = find_fn("pure_core");
    let driver_idx = find_fn("fs_clock_driver");
    let pure_key = program.function(pure_idx).key;
    let driver_key = program.function(driver_idx).key;
    assert!(checked[pure_idx].as_ref().expect("pure_core checked").diags.is_empty());
    assert!(checked[driver_idx].as_ref().expect("fs_clock_driver checked").diags.is_empty());

    // 3. Enforce `#![blaze::sans_io]` via `check_sans_io_effects`: pure computation passes, while
    //    `SyscallSet::NET` and `EffectSet::SUSPEND` are refused.
    let sans_io_attrs = TypeAttrs(TypeAttrs::SANS_IO);
    assert_eq!(check_sans_io_effects(pure_key, sans_io_attrs, EffectSet::PURE, SyscallSet::EMPTY), Ok(()));
    assert_eq!(
        check_sans_io_effects(pure_key, sans_io_attrs, EffectSet::PURE, SyscallSet::NET),
        Err(SansIoBodyFinding {
            owner: pure_key,
            error: SyscallPolicyError::SansIoViolation {
                effects: EffectSet::PURE,
                syscalls: SyscallSet::NET,
            },
        }),
    );
    assert_eq!(
        check_sans_io_effects(pure_key, sans_io_attrs, EffectSet::SUSPEND, SyscallSet::EMPTY),
        Err(SansIoBodyFinding {
            owner: pure_key,
            error: SyscallPolicyError::SansIoViolation {
                effects: EffectSet::SUSPEND,
                syscalls: SyscallSet::EMPTY,
            },
        }),
    );

    // 4. Enforce `#[blaze::syscalls(fs, clock)]` via `check_body_syscall_policy`: accepts
    //    `FS | CLOCK` and refuses `PROC`.
    let fs = SyscallSet::from_word("fs").expect("`fs` syscall domain");
    let clock = SyscallSet::from_word("clock").expect("`clock` syscall domain");
    assert_eq!(fs, SyscallSet::FS);
    assert_eq!(clock, SyscallSet::CLOCK);
    let allowed = fs.union(clock);
    assert_eq!(check_body_syscall_policy(driver_key, allowed, allowed), Ok(()));
    assert_eq!(check_body_syscall_policy(driver_key, SyscallSet::FS, allowed), Ok(()));
    assert_eq!(check_body_syscall_policy(driver_key, SyscallSet::CLOCK, allowed), Ok(()));
    assert_eq!(
        check_body_syscall_policy(driver_key, SyscallSet::PROC, allowed),
        Err(SansIoBodyFinding {
            owner: driver_key,
            error: SyscallPolicyError::SyscallExceedsPolicy {
                required: SyscallSet::PROC,
                allowed,
            },
        }),
    );
}

/// Real `#![no_core]` `#![blaze::sans_io]` Rust source attempting `.do` suspension inside a
/// sans-IO core, triggering `suspend-in-sans-io` (`TypedFinding::SuspendInSansIo`).
const BAD_SANS_IO_DO_PROGRAM: &str = r#"#![feature(no_core, lang_items, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}

trait Request {
    type Answer;
}

struct Ping(u64);
impl Copy for Ping {}

impl Request for Ping {
    type Answer = u64;
}

fn bad_core(req: Ping) -> u64 {
    req.do
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = bad_core(Ping(1));
    0
}
"#;

/// Exact control for [`BAD_SANS_IO_DO_PROGRAM`] without `#![blaze::sans_io]`: the same `.do`
/// body must pass clean with zero findings.
const OK_DRIVER_DO_PROGRAM: &str = r#"#![feature(no_core, lang_items, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u64 {}

trait Request {
    type Answer;
}

struct Ping(u64);
impl Copy for Ping {}

impl Request for Ping {
    type Answer = u64;
}

fn bad_core(req: Ping) -> u64 {
    req.do
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = bad_core(Ping(1));
    0
}
"#;

#[test]
fn real_rust_do_inside_sans_io_body_is_rejected_by_typeck() {
    let alloc = Alloc::global();
    let tc = Toolchain::none(alloc);

    // 1. With `#![blaze::sans_io]`: `.do` inside `bad_core` emits `suspend-in-sans-io` through
    //    both `ProjectWorld::check` and `Front2Program::check_all`.
    {
        let root = "/sans-io-do-bad/src/main.rs";
        let files = [(root.to_owned(), BAD_SANS_IO_DO_PROGRAM.to_owned())];
        let user = UserCrate {
            name: "sans_io_do_bad",
            dir: "/sans-io-do-bad/",
            root,
            files: &files,
            edition: edition::PackageEdition::of_manifest(EDITION).expect("blaze edition"),
            panic: Panic::Abort,
        };
        let inputs = Inputs::new(&user, &tc).expect("inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
        let sources = [SourceFile { path: "src/main.rs", text: BAD_SANS_IO_DO_PROGRAM }];

        let check_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("check frame");
        let mut world = ProjectWorld::new(&names, &check_frame, &sources, alloc).expect("project world");
        let bad_lib_key = names
            .items(0)
            .find_map(|(k, _, _)| (names.item_text(k)?.name == Some("bad_core")).then_some(k))
            .expect("bad_core item");
        let verdict = world.check(BodyRef::Free(bad_lib_key), alloc);
        assert!(verdict.is_clean(), "bad_core should finish typecheck with a typed lint finding: {verdict:?}");
        assert_eq!(verdict.findings.len(), 1, "expected 1 suspend-in-sans-io finding: {:?}", verdict.findings);
        assert_eq!(verdict.findings[0].lint, "suspend-in-sans-io");
        assert!(
            verdict.findings[0].text.contains("cannot suspend via `.do`"),
            "unexpected finding text: {}",
            verdict.findings[0].text,
        );

        let prog_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("program frame");
        let mut program = Front2Program::new(&names, &prog_frame, &sources, alloc).expect("front2 program");
        let checked = program.check_all().expect("check_all");
        let bad_idx = (0..program.fn_count())
            .find(|&i| {
                let path = program.function(i).path;
                path == "bad_core" || path.ends_with("::bad_core")
            })
            .expect("bad_core row in Front2Program");
        let bad_checked = checked[bad_idx].as_ref().expect("bad_core checked record");
        assert!(bad_checked.suspends, "bad_core must mark checked.suspends == true");
        assert_eq!(bad_checked.findings.len(), 1, "expected 1 TypedFinding: {:?}", bad_checked.findings);
        assert_eq!(bad_checked.findings[0].lint(), "suspend-in-sans-io");
        assert!(
            matches!(bad_checked.findings[0], TypedFinding::SuspendInSansIo { .. }),
            "expected TypedFinding::SuspendInSansIo, got {:?}",
            bad_checked.findings[0],
        );
    }

    // 2. Control without `#![blaze::sans_io]`: the exact same `.do` function passes clean with
    //    zero findings.
    {
        let root = "/sans-io-do-ok/src/main.rs";
        let files = [(root.to_owned(), OK_DRIVER_DO_PROGRAM.to_owned())];
        let user = UserCrate {
            name: "sans_io_do_ok",
            dir: "/sans-io-do-ok/",
            root,
            files: &files,
            edition: edition::PackageEdition::of_manifest(EDITION).expect("blaze edition"),
            panic: Panic::Abort,
        };
        let inputs = Inputs::new(&user, &tc).expect("inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
        let sources = [SourceFile { path: "src/main.rs", text: OK_DRIVER_DO_PROGRAM }];

        let check_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("check frame");
        let mut world = ProjectWorld::new(&names, &check_frame, &sources, alloc).expect("project world");
        let ok_lib_key = names
            .items(0)
            .find_map(|(k, _, _)| (names.item_text(k)?.name == Some("bad_core")).then_some(k))
            .expect("bad_core item");
        let verdict = world.check(BodyRef::Free(ok_lib_key), alloc);
        assert!(verdict.is_clean(), "driver `.do` must typecheck clean: {verdict:?}");
        assert!(verdict.findings.is_empty(), "driver `.do` must emit 0 findings: {:?}", verdict.findings);

        let prog_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("program frame");
        let mut program = Front2Program::new(&names, &prog_frame, &sources, alloc).expect("front2 program");
        let checked = program.check_all().expect("check_all");
        let ok_idx = (0..program.fn_count())
            .find(|&i| {
                let path = program.function(i).path;
                path == "bad_core" || path.ends_with("::bad_core")
            })
            .expect("bad_core row in Front2Program");
        let ok_checked = checked[ok_idx].as_ref().expect("bad_core checked record");
        assert!(ok_checked.suspends, "driver `.do` must mark checked.suspends == true");
        assert!(ok_checked.findings.is_empty(), "driver `.do` must emit 0 TypedFindings: {:?}", ok_checked.findings);
    }
}
