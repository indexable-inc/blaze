//! An embedder's side of the evaluator: the calls it claims (asked once per function, run with
//! the embedder taken out so a call back into the program runs the real body, or given back
//! to run in place), its calls into
//! the program directly and through a function pointer, the program's memory, `errno`, the
//! budget, and the statics a run changed.

use std::{cell::RefCell, rc::Rc};

use base_db::{ConstEvalBudget, Db};
use hir_def::FunctionId;
use hir_ty::next_solver::GenericArgs;
use hir_ty_mir::mir::{
    Evaluator, MirEvalError,
    foreign::{Claimed, Foreign},
};
use hir_expand::fixture;

use crate::{db::TestDB, function};

const PROGRAM: &str = r#"
//- minicore: sized, copy, add, mul, eq
//- /main.rs cfg:target_os=linux,target_arch=x86_64
extern "C" {
    fn __errno_location() -> *mut i32;
}

fn host(x: u32) -> u32 { x + 100 }
fn guest(x: u32) -> u32 { host(x) + host(x) }
fn double(x: u32) -> u32 { x * 2 }
fn pointer() -> fn(u32) -> u32 { double }
fn read(p: *const u32) -> u32 { unsafe { *p } }
fn errno() -> i32 { unsafe { *__errno_location() } }
fn spin() -> u32 {
    let mut i = 0;
    loop {
        if i == 50 {
            return i;
        }
        i = i + 1;
    }
}

static mut COUNT: u32 = 7;
static mut PTR: *const u32 = 0 as *const u32;
static FIXED: u32 = 1;
fn bump() -> u32 {
    unsafe {
        COUNT = COUNT + 1;
        PTR = &raw const COUNT;
        COUNT + FIXED
    }
}
"#;

/// How [`Host`] answers a call of `host`.
#[derive(Clone, Copy)]
enum Mode {
    /// Ten times its argument.
    Answer,
    /// Twice what the program's `host` gives, through a nested call.
    Nested,
    /// Not at all: the evaluator runs the program's `host`.
    Decline,
}

/// Claims `host` and answers it as `mode` says.
struct Host {
    host: FunctionId,
    mode: Mode,
    asked: Rc<RefCell<Vec<FunctionId>>>,
}

impl<'db> Foreign<'db> for Host {
    fn claims(&mut self, _: &'db Db, func: FunctionId) -> bool {
        self.asked.borrow_mut().push(func);
        func == self.host
    }

    fn call(
        &mut self,
        eval: &mut Evaluator<'db>,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        args: &[&[u8]],
        ret: &mut [u8],
    ) -> Result<Claimed, MirEvalError<'db>> {
        let value = match self.mode {
            Mode::Answer => u32_of(args[0]) * 10,
            Mode::Nested => u32_of(&eval.call(func, generic_args, args)?) * 2,
            Mode::Decline => return Ok(Claimed::Declined),
        };
        ret.copy_from_slice(&value.to_le_bytes());
        Ok(Claimed::Answered)
    }
}

fn u32_of(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

/// Runs `test` with an evaluator of `PROGRAM`'s `owner` and the database.
fn with_evaluator(owner: &str, test: impl FnOnce(&TestDB, Evaluator<'_>)) {
    let fx = fixture::db(PROGRAM);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let owner = function(&db, owner);
        let eval = Evaluator::new(&db, owner.into(), None).expect("an evaluator");
        test(&db, eval);
    })
}

fn guest_with(mode: Mode) -> (u32, Vec<FunctionId>, FunctionId) {
    let mut out = None;
    with_evaluator("guest", |db, mut eval| {
        let host = function(db, "host");
        let asked = Rc::default();
        // clone: an `Rc`
        eval.set_foreign(Some(Box::new(Host { host, mode, asked: Rc::clone(&asked) })));
        let guest = function(db, "guest");
        let result = eval.call(guest, GenericArgs::empty(), &[&5u32.to_le_bytes()]);
        let result = u32_of(&result.expect("guest runs"));
        out = Some((result, asked.take(), host));
    });
    out.expect("ran")
}

#[test]
fn claimed_calls_are_the_embedders_and_asked_once() {
    let (result, asked, host) = guest_with(Mode::Answer);
    assert_eq!(result, 100);
    assert_eq!(asked.iter().filter(|&&it| it == host).count(), 1, "{asked:?}");
}

#[test]
fn a_claimed_call_calls_back_without_the_embedder() {
    // Each `host(5)` is the embedder's, which runs the program's `host` (105) and doubles it;
    // the second call is claimed again, so the embedder was put back.
    let (result, _, _) = guest_with(Mode::Nested);
    assert_eq!(result, 420);
}

#[test]
fn a_declined_call_runs_in_place() {
    // Each `host(5)` is claimed, given back, and runs the program's `host`: 105 twice.
    let (result, asked, host) = guest_with(Mode::Decline);
    assert_eq!(result, 210);
    assert_eq!(asked.iter().filter(|&&it| it == host).count(), 1, "{asked:?}");
}

#[test]
fn fn_pointers_are_the_programs() {
    with_evaluator("pointer", |db, mut eval| {
        let double = function(db, "double");
        let made = eval.fn_pointer(double, GenericArgs::empty()).expect("a pointer");
        let cast = eval.call(function(db, "pointer"), GenericArgs::empty(), &[]).expect("pointer");
        assert_eq!(made, cast);
        let result = eval.call_pointer(&made, &[&21u32.to_le_bytes()]).expect("double runs");
        assert_eq!(u32_of(&result), 42);
    });
}

#[test]
fn memory_is_the_programs() {
    with_evaluator("read", |db, mut eval| {
        let addr = eval.alloc_bytes(&7u32.to_le_bytes(), 4).expect("allocated");
        eval.write_bytes(addr, &9u32.to_le_bytes()).expect("written");
        assert_eq!(eval.read_bytes(addr, 4).expect("read"), 9u32.to_le_bytes());
        let read = function(db, "read");
        let result = eval.call(read, GenericArgs::empty(), &[&addr.to_le_bytes()]);
        assert_eq!(u32_of(&result.expect("read runs")), 9);
    });
}

#[test]
fn errno_is_the_running_threads() {
    with_evaluator("errno", |db, mut eval| {
        eval.set_errno(5).expect("set");
        let result = eval.call(function(db, "errno"), GenericArgs::empty(), &[]);
        assert_eq!(i32::from_le_bytes(result.expect("errno runs").try_into().unwrap()), 5);
    });
}

#[test]
fn the_budget_is_the_embedders() {
    with_evaluator("spin", |db, mut eval| {
        eval.set_budget(ConstEvalBudget { steps: 10, memory: u64::MAX });
        let result = eval.call(function(db, "spin"), GenericArgs::empty(), &[]);
        let mut error = result.expect_err("over the budget");
        while let MirEvalError::InFunction(inner, _) = error {
            error = *inner;
        }
        assert!(matches!(error, MirEvalError::OverBudget { .. }), "{error:?}");
    });
    with_evaluator("spin", |db, mut eval| {
        eval.set_budget(ConstEvalBudget { steps: u64::MAX, memory: u64::MAX });
        let result = eval.call(function(db, "spin"), GenericArgs::empty(), &[]);
        assert_eq!(u32_of(&result.expect("spin runs")), 50);
    });
}

#[test]
fn changed_statics_are_reported_from_their_preset() {
    with_evaluator("bump", |db, mut eval| {
        let krate = fixture::test_crate(&db);
        let count = hir_def::test_items::static_item(db, krate, "COUNT");
        let ptr = hir_def::test_items::static_item(db, krate, "PTR");
        eval.track_statics();
        eval.preset_static(count, &40u32.to_le_bytes());
        let result = eval.call(function(db, "bump"), GenericArgs::empty(), &[]);
        assert_eq!(u32_of(&result.expect("bump runs")), 42);
        let mut changed = Vec::new();
        eval.changed_statics(&mut changed).expect("read");
        changed.sort_by_key(|&(id, ..)| id != count);
        let [(first, bytes, holds), (second, _, points)] = &changed[..] else {
            panic!("COUNT and PTR changed, FIXED did not: {changed:?}");
        };
        assert_eq!((*first, bytes.as_slice(), *holds), (count, &41u32.to_le_bytes()[..], false));
        assert_eq!((*second, *points), (ptr, true));
    });
}
