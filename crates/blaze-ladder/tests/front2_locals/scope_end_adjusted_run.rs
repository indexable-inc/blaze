//! Regression tests for `blaze-build` `Task::ScopeEnd` and `Task::TempScopeEnd` adjusted-repr
//! settling (`walk.rs`, `walk/matches.rs`, `walk/lets.rs`, `walk/temps.rs`).
//!
//! When an arm or branch body (`arm.body`, `then`, `let_guard` `body`) undergoes a size-changing
//! adjustment (e.g. `AdjustKind::Unsize` from an 8-byte `Box<T>` / `PtrBox<T>` aggregate to a
//! 16-byte `Box<dyn Tr>` / `PtrBox<dyn Tr>`, or `&T -> &dyn Tr`, or `&[u8; 4] -> &[u8]`) AND the
//! closing scope has a pending drop, `settle_scope` must copy the value at its **adjusted** repr
//! (`adjusted_repr`), not its pre-adjustment `expr_repr` (which truncated 16-byte fat pointers to
//! 8 bytes and left uninitialized stack garbage in the vtable/length word).
//!
//! Conversely, `Expr::Block` and single-arm irrefutable `match` (`try_match_whole_bind`) run
//! `Task::ScopeEnd` *before* the enclosing expression's own `Task::Adjust`, so those two sites
//! must keep using the unadjusted `expr_repr(e)`.

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types, coerce_unsized)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

/// Smart-pointer aggregate shaped like `alloc::boxed::Box<T>`:
/// `PtrBox<Concrete>` is an 8-byte `Repr::Agg`, while `PtrBox<dyn Tr>` is a 16-byte `Repr::Agg`.
struct PtrBox<T: PointeeSized>(*const T);

impl<T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<PtrBox<U>> for PtrBox<T> {}

impl<T: PointeeSized> PtrBox<T> {
    fn as_ref(&self) -> &T {
        unsafe { &*self.0 }
    }
}

trait Tr {
    fn val(&self) -> u32;
}

struct Concrete(u32);
impl Tr for Concrete {
    fn val(&self) -> u32 {
        self.0
    }
}

struct Other(u32);
impl Tr for Other {
    fn val(&self) -> u32 {
        self.0
    }
}

struct Guard {
    id: u32,
    log: *mut u32,
}

impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            *self.log = *self.log + self.id;
        }
    }
}

fn guard(id: u32, log: *mut u32) -> Guard {
    Guard { id, log }
}
";

fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &text },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
    ];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared())
        .map_err(|e| format!("{e}").replace('\n', " "))
}

/// 1. Match arm unsizing `PtrBox<Concrete> -> PtrBox<dyn Tr>` with a droppable binding in both
///    switch-based `match` (`walk.rs`) and test-based `match` (`walk/matches.rs`), via both
///    implicit coercion and explicit `as` cast.
#[test]
fn match_arm_unsizes_box_to_dyn_with_drop_binding() {
    let got = run("
fn pick_implicit(opt: Option<Guard>, c: &Concrete, o: &Other) -> PtrBox<dyn Tr> {
    let fallback: PtrBox<dyn Tr> = PtrBox(o as *const Other);
    match opt {
        Some(_droppable) => PtrBox(c as *const Concrete),
        None => fallback,
    }
}

fn pick_cast(opt: Option<Guard>, c: &Concrete, o: &Other) -> PtrBox<dyn Tr> {
    match opt {
        Some(_droppable) => PtrBox(c as *const Concrete) as PtrBox<dyn Tr>,
        None => PtrBox(o as *const Other) as PtrBox<dyn Tr>,
    }
}

fn pick_tested(opt: Option<Guard>, tag: u32, c: &Concrete, o: &Other) -> PtrBox<dyn Tr> {
    let fallback: PtrBox<dyn Tr> = PtrBox(o as *const Other);
    match (opt, tag) {
        (Some(_droppable), 1) => PtrBox(c as *const Concrete),
        _ => fallback,
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let c = Concrete(42);
    let o = Other(7);
    let mut drops = 0u32;
    let p = &mut drops as *mut u32;

    let a = pick_implicit(Some(guard(1, p)), &c, &o);
    let b = pick_implicit(None, &c, &o);
    let d = pick_cast(Some(guard(2, p)), &c, &o);
    let e = pick_tested(Some(guard(4, p)), 1, &c, &o);
    let f = pick_tested(Some(guard(8, p)), 2, &c, &o);

    let sum = a.as_ref().val() + b.as_ref().val() + d.as_ref().val() + e.as_ref().val() + f.as_ref().val();
    // sum = 42 + 7 + 42 + 42 + 7 = 140; drops = 1 + 2 + 4 + 8 = 15
    if drops == 15 { sum as i32 } else { -1 }
}
");
    assert_eq!(got, Ok(140));
}

/// 2. `if let` with a temporary scrutinee whose type/size differs from the `then` branch AND
///    needs `Drop`, while `then` unsizes `PtrBox<Concrete> -> PtrBox<dyn Tr>` (exercises both
///    `ScopeEnd { expr: then, adjusted: true }` and `TempScopeEnd { anchor: cond, value: then }`).
#[test]
fn if_let_unsizes_box_to_dyn_with_temp_scrutinee_and_drop_binding() {
    let got = run("
enum BigOpt {
    Some(Guard, Guard, [u8; 24]),
    None(Guard),
}

fn make_temp(take: bool, log: *mut u32) -> BigOpt {
    if take {
        BigOpt::Some(guard(1, log), guard(2, log), [9u8; 24])
    } else {
        BigOpt::None(guard(4, log))
    }
}

fn pick_if_let(take: bool, log: *mut u32, c: &Concrete, o: &Other) -> PtrBox<dyn Tr> {
    let fallback: PtrBox<dyn Tr> = PtrBox(o as *const Other);
    if let BigOpt::Some(_droppable, _, _) = make_temp(take, log) {
        PtrBox(c as *const Concrete)
    } else {
        fallback
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let c = Concrete(42);
    let o = Other(7);
    let mut drops = 0u32;
    let p = &mut drops as *mut u32;

    let a = pick_if_let(true, p, &c, &o);
    let b = pick_if_let(false, p, &c, &o);
    let sum = a.as_ref().val() + b.as_ref().val();
    // sum = 42 + 7 = 49; drops = (1 + 2) + 4 = 7
    if drops == 7 { sum as i32 } else { -1 }
}
");
    assert_eq!(got, Ok(49));
}

/// 3. Match arms with boolean guards (creating a droppable temporary) AND `if let` guards
///    (creating a droppable scrutinee temporary + guard binding), returning both unsized
///    `PtrBox<dyn Tr>` and unsized `&[u8]` (coerced from `&[u8; 4]`).
#[test]
fn match_guards_unsize_dyn_and_slice_with_temp_and_drop_bindings() {
    let got = run("
struct Flag {
    ok: bool,
    _g: Guard,
}

fn check_temp(ok: bool, id: u32, log: *mut u32) -> bool {
    let f = Flag { ok, _g: guard(id, log) };
    f.ok
}

fn make_guard_opt(take: bool, id: u32, log: *mut u32) -> Option<Guard> {
    if take { Some(guard(id, log)) } else { None }
}

fn pick_guarded_box(opt: Option<Guard>, ok: bool, log: *mut u32, c: &Concrete, o: &Other) -> PtrBox<dyn Tr> {
    let fallback: PtrBox<dyn Tr> = PtrBox(o as *const Other);
    match opt {
        Some(_droppable) if check_temp(ok, 2, log) => PtrBox(c as *const Concrete),
        _ => fallback,
    }
}

fn pick_guarded_slice<'a>(opt: Option<Guard>, ok: bool, log: *mut u32, a: &'a [u8; 4], b: &'a [u8; 2]) -> &'a [u8] {
    let fallback: &'a [u8] = b;
    match opt {
        Some(_droppable) if check_temp(ok, 4, log) => a,
        _ => fallback,
    }
}

fn pick_let_guard_box(opt: Option<Guard>, ok: bool, log: *mut u32, c: &Concrete, o: &Other) -> PtrBox<dyn Tr> {
    let fallback: PtrBox<dyn Tr> = PtrBox(o as *const Other);
    match opt {
        Some(_droppable) if let Some(_gbind) = make_guard_opt(ok, 8, log) => PtrBox(c as *const Concrete),
        _ => fallback,
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let c = Concrete(42);
    let o = Other(7);
    let arr4 = [1u8, 2, 3, 4];
    let arr2 = [10u8, 20];
    let mut drops = 0u32;
    let p = &mut drops as *mut u32;

    let r1 = pick_guarded_box(Some(guard(1, p)), true, p, &c, &o);
    let s1 = pick_guarded_slice(Some(guard(16, p)), true, p, &arr4, &arr2);
    let r2 = pick_let_guard_box(Some(guard(32, p)), true, p, &c, &o);

    let slice_sum = (s1[0] as u32) + (s1[1] as u32) + (s1[2] as u32) + (s1[3] as u32);
    let total = r1.as_ref().val() + slice_sum + r2.as_ref().val();
    // total = 42 + 10 + 42 = 94; drops = (1+2) + (16+4) + (32+8) = 63
    if drops == 63 { total as i32 } else { -1 }
}
");
    assert_eq!(got, Ok(94));
}

/// 4. `Expr::Block` and single-arm irrefutable `match` (`try_match_whole_bind`) where the outer
///    expression itself undergoes an unsizing coercion (`Task::ScopeEnd` runs *before*
///    `Task::Adjust`), plus inner tail coercion.
#[test]
fn block_and_single_arm_match_outer_unsizing_coercion() {
    let got = run("
fn take_dyn_ref(r: &dyn Tr) -> u32 {
    r.val()
}

fn take_dyn_box(b: PtrBox<dyn Tr>) -> u32 {
    b.as_ref().val()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let c = Concrete(10);
    let mut drops = 0u32;
    let p = &mut drops as *mut u32;

    // Inner coercion on tail + drop guard in block
    let r1: &dyn Tr = {
        let _d = guard(1, p);
        &c
    };
    // Outer coercion on block expression itself (passed as argument / cast)
    let v2 = take_dyn_ref({
        let _d = guard(2, p);
        &c
    });
    let b3: PtrBox<dyn Tr> = ({
        let _d = guard(4, p);
        PtrBox(&c as *const Concrete)
    }) as PtrBox<dyn Tr>;
    let v4 = take_dyn_box({
        let _d = guard(8, p);
        PtrBox(&c as *const Concrete)
    });
    // Single-arm irrefutable match (`try_match_whole_bind`) with implicit and outer coercion
    let b5: PtrBox<dyn Tr> = match guard(16, p) {
        _d => PtrBox(&c as *const Concrete),
    };
    let b6: PtrBox<dyn Tr> = (match guard(32, p) {
        _d => PtrBox(&c as *const Concrete),
    }) as PtrBox<dyn Tr>;

    let sum = r1.val() + v2 + b3.as_ref().val() + v4 + b5.as_ref().val() + b6.as_ref().val();
    // sum = 60; drops = 1 + 2 + 4 + 8 + 16 + 32 = 63
    if drops == 63 { sum as i32 } else { -1 }
}
");
    assert_eq!(got, Ok(60));
}
