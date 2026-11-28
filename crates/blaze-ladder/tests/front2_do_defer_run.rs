//! End-to-end real-Rust-source integration tests for lexical `defer { ... }` blocks and one-shot
//! postfix `.do` zero-`Pin` sans-IO machines through the full `blaze-ladder` pipeline
//! (`parser` -> `blaze-scope` -> `blaze-lower` -> `blaze-typeck` -> `blaze-build` ->
//! `blaze-lir::coroutine_machine` / in-process execution).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_build::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use blaze_build::{
    Abi, Adts, BuildError, OverflowChecks, Panic, Subst, UbChecks, build_do_machine_body, split_do_machine_body,
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
use blaze_typeck::{Adt, ExprId, ItemKey, Name, PTy, PortableTys, TypedFinding};

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
