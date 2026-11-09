//! blaze's `#![feature(blaze_comptime)]` in the evaluator (docs/v2/plan/design/comptime.md,
//! "Phase 2b"): calls through `dyn` and function pointers in const, checked when evaluated, and
//! `const static { .. }` blocks, which run any code that can run at compile time.

use hir_def::{ItemContainerId, Lookup, signatures::FunctionSignature};
use hir_expand::fixture;

use super::{check_fail, check_number, check_str, eval_goal};
use crate::{
    consteval::ConstEvalError,
    display::DisplayTarget,
    mir::{AddressRead, ComptimeError, ComptimeErrorKind, Dispatch, MirEvalError},
    mir_pretty::comptime_error_text,
    test_db::TestDB,
};

/// The `ComptimeError` an evaluation stopped with, through the constants and frames it is in.
fn comptime_of<'a, 'db>(mut error: &'a ConstEvalError<'db>) -> Option<&'a ComptimeError<'db>> {
    loop {
        let ConstEvalError::MirEvalError(mut inner) = error else { return None };
        loop {
            match inner {
                MirEvalError::InFunction(it, _) => inner = &**it,
                MirEvalError::ConstEvalError(_, it) => {
                    error = &**it;
                    break;
                }
                MirEvalError::Comptime(it) => return Some(&**it),
                _ => return None,
            }
        }
    }
}

/// Evaluates `GOAL` of `fixture`, which must stop with a `ComptimeError`, and hands it to
/// `check` with the text a diagnostic gives it.
#[track_caller]
fn check_refused(fixture: &str, check: impl FnOnce(&TestDB, &ComptimeError<'_>, &str)) {
    let fx = fixture::db(fixture);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let error = match eval_goal(&db, *file_ids.last().unwrap()) {
            Ok(_) => panic!("`GOAL` evaluated; a `ComptimeError` was expected"),
            Err(error) => error,
        };
        let Some(refused) = comptime_of(&error) else {
            panic!("`GOAL` failed with no `ComptimeError`: {error:?}");
        };
        let krate = *base_db::all_crates(&db).last().expect("the test crate");
        let text = comptime_error_text(&db, refused, DisplayTarget::from_crate(&db, krate));
        assert!(refused.at.is_some(), "the refusal is placed: {text}");
        check(&db, refused, &text);
    })
}

/// The function a `NonConstTarget` names, by its name and whether it is an impl's method.
fn named(db: &TestDB, refused: &ComptimeError<'_>) -> (String, bool) {
    let ComptimeErrorKind::NonConstTarget { callee: either::Either::Left(func), .. } =
        &refused.kind
    else {
        panic!("not a function reached through a pointer or vtable: {refused:?}");
    };
    let name = FunctionSignature::of(db, *func).name.as_str().to_owned();
    (name, matches!(func.lookup(db).container, ItemContainerId::ImplId(_)))
}

/// A `const trait` and a plain trait, each implemented `const` or not, and `const fn`s calling
/// through `dyn` and a function pointer.
const DISPATCH: &str = r#"
pub const trait Get {
    fn get(&self) -> u8;
}
pub trait Plain {
    fn get(&self) -> u8;
}
pub struct Seven;
const impl Get for Seven {
    fn get(&self) -> u8 { 7 }
}
pub struct Eight;
impl Get for Eight {
    fn get(&self) -> u8 { 8 }
}
impl Plain for Seven {
    fn get(&self) -> u8 { 9 }
}
pub const fn by_dyn(x: &dyn Get) -> u8 { x.get() }
pub const fn by_plain(x: &dyn Plain) -> u8 { x.get() }
pub const fn by_pointer(f: fn() -> u8) -> u8 { f() }
pub const fn seven() -> u8 { 7 }
pub fn eight() -> u8 { 8 }
"#;

fn dispatch(feature: bool, goal: &str) -> String {
    let features = match feature {
        true => "blaze_comptime, const_trait_impl",
        false => "const_trait_impl",
    };
    format!(
        "//- minicore: sized, coerce_unsized, dispatch_from_dyn, fn\n#![feature({features})]\n\
         {DISPATCH}\nconst GOAL: u8 = {goal};\n"
    )
}

/// A `dyn` call of a method of a `const impl` evaluates in a `const fn`; one of a plain impl
/// is refused when it is reached, naming the impl's method and the vtable it came through.
#[test]
fn a_dyn_call_in_const_is_checked_when_evaluated() {
    check_number(&dispatch(true, "by_dyn(&Seven)"), 7);
    check_refused(&dispatch(true, "by_dyn(&Eight)"), |db, refused, text| {
        assert_eq!(named(db, refused), ("get".to_owned(), true), "{text}");
        let ComptimeErrorKind::NonConstTarget { through, bounds, .. } = &refused.kind else {
            unreachable!()
        };
        assert!(matches!(through, Dispatch::Vtable { .. }), "{text}");
        assert!(!bounds, "the impl is not const, no bound failed: {text}");
        assert!(text.contains("vtable") && text.contains("Get"), "{text}");
        assert!(text.contains("Eight"), "the impl's method is named: {text}");
    });
    check_refused(&dispatch(true, "by_plain(&Seven)"), |db, refused, text| {
        assert_eq!(named(db, refused), ("get".to_owned(), true), "{text}");
        assert!(text.contains("Plain"), "{text}");
    });
}

/// A call through a function pointer of a `const fn` evaluates; of a plain `fn`, it is refused
/// naming it.
#[test]
fn a_fn_pointer_call_in_const_is_checked_when_evaluated() {
    check_number(&dispatch(true, "by_pointer(seven)"), 7);
    check_refused(&dispatch(true, "by_pointer(eight)"), |db, refused, text| {
        assert_eq!(named(db, refused), ("eight".to_owned(), false), "{text}");
        let ComptimeErrorKind::NonConstTarget { through, .. } = &refused.kind else {
            unreachable!()
        };
        assert_eq!(*through, Dispatch::FnPointer, "{text}");
        assert!(text.contains("`eight`") && text.contains("function pointer"), "{text}");
    });
}

/// Without the feature the evaluator runs these calls as before: the static check refuses
/// them (`tests/mir/const_check.rs`), not the evaluation.
#[test]
fn without_the_feature_the_evaluator_does_not_check_dispatch() {
    check_number(&dispatch(false, "by_dyn(&Eight)"), 8);
    check_number(&dispatch(false, "by_plain(&Seven)"), 9);
    check_number(&dispatch(false, "by_pointer(eight)"), 8);
}

/// What `format!` and `String` do, reduced: a buffer that grows over the program's allocator,
/// a `Write` trait called through `dyn`, an argument's formatter as a function pointer (as
/// `fmt::rt::Argument` holds it), none of it `const`, and `leak`.
const FORMAT: &str = r#"
extern "Rust" {
    #[rustc_allocator]
    fn __rust_alloc(size: usize, align: usize) -> *mut u8;
    #[rustc_reallocator]
    fn __rust_realloc(ptr: *mut u8, old_size: usize, align: usize, new_size: usize) -> *mut u8;
    #[rustc_deallocator]
    fn __rust_dealloc(ptr: *mut u8, size: usize, align: usize);
}
#[rustc_intrinsic]
pub const unsafe fn offset<Ptr, Delta>(dst: Ptr, offset: Delta) -> Ptr;

#[repr(C)]
struct Raw { ptr: *const u8, len: usize }

pub struct Buf { ptr: *mut u8, cap: usize, len: usize }
impl Buf {
    fn new() -> Buf {
        Buf { ptr: unsafe { __rust_alloc(4, 1) }, cap: 4, len: 0 }
    }
    fn push(&mut self, byte: u8) {
        if self.len == self.cap {
            self.ptr = unsafe { __rust_realloc(self.ptr, self.cap, 1, self.cap * 2) };
            self.cap *= 2;
        }
        unsafe { *offset(self.ptr, self.len) = byte };
        self.len += 1;
    }
    /// `String::leak`: the text, which nothing frees.
    fn leak(self) -> &'static str {
        unsafe { core::mem::transmute(Raw { ptr: self.ptr, len: self.len }) }
    }
}
pub trait Write {
    fn write_str(&mut self, s: &str);
}
impl Write for Buf {
    fn write_str(&mut self, s: &str) {
        let raw: Raw = unsafe { core::mem::transmute(s) };
        let mut i = 0;
        while i < raw.len {
            self.push(unsafe { *offset(raw.ptr, i) });
            i += 1;
        }
    }
}
pub struct Arg<'a> { value: &'a str, fmt: fn(&str, &mut dyn Write) }
fn display(s: &str, out: &mut dyn Write) {
    out.write_str(s)
}
fn write(out: &mut dyn Write, pieces: [&str; 3], args: [Arg<'_>; 2]) {
    let mut i = 0;
    while i < 3 {
        out.write_str(pieces[i]);
        if i < 2 {
            (args[i].fmt)(args[i].value, out);
        }
        i += 1;
    }
}
fn format(pieces: [&str; 3], args: [Arg<'_>; 2]) -> Buf {
    let mut out = Buf::new();
    write(&mut out, pieces, args);
    out
}
const SYM: &str = "spin";
"#;

fn format_fixture(goal_ty: &str, goal: &str) -> String {
    format!(
        "//- minicore: sized, coerce_unsized, dispatch_from_dyn, fn, transmute, index, slice\n\
         #![feature(blaze_comptime)]\n{FORMAT}\nconst GOAL: {goal_ty} = {goal};\n"
    )
}

const PAUSE: &str = "{ \
     let args = [Arg { value: SYM, fmt: display }, Arg { value: SYM, fmt: display }]; \
     format([\".globl \", \"\\n\", \":\\npause\\nret\\n\"], args).leak() }";

/// The target of the design: `const PAUSE: &str = const static { format!(..).leak() }`. Non-const
/// functions, `dyn` and function-pointer calls run; the leaked buffer, reached through the
/// `&str`, is made global and is the constant's.
#[test]
fn a_const_static_block_formats_and_leaks() {
    let goal = format!("const static {PAUSE}");
    check_str(&format_fixture("&str", &goal), ".globl spin\nspin:\npause\nret\n");
}

/// The same code in a plain constant keeps memory the program would free: refused, as before.
/// Only `const static` makes the leaked buffer the constant's.
#[test]
fn without_static_the_leaked_buffer_is_refused() {
    check_fail(&format_fixture("&str", PAUSE), |e| {
        let heap = MirEvalError::HeapInValue { of_const_allocate: false };
        matches!(&e, ConstEvalError::MirEvalError(it) if *it == heap)
    });
}

/// A `const static` block whose value owns its heap (a `Buf` by value, as a `String`) is
/// refused: each use of the constant would copy the owner, and a drop free the constant's
/// memory.
#[test]
fn a_const_static_block_may_not_own_its_heap() {
    let fixture = format_fixture("Buf", "const static { let mut b = Buf::new(); b.push(1); b }");
    check_fail(&fixture, |e| {
        let mut e = &e;
        while let ConstEvalError::MirEvalError(MirEvalError::ConstEvalError(_, inner)) = e {
            e = &**inner;
        }
        let heap = MirEvalError::HeapInValue { of_const_allocate: false };
        matches!(e, ConstEvalError::MirEvalError(it) if *it == heap)
    });
}

/// A `const static` block that does I/O is refused at the call, naming the function.
#[test]
fn a_const_static_block_may_not_do_io() {
    let fixture = r#"
//- minicore: sized
#![feature(blaze_comptime)]
extern "C" {
    fn write(fd: i32, buf: *const u8, count: usize) -> usize;
}
const GOAL: usize = const static {
    let text = b'.';
    unsafe { write(1, &text, 1) }
};
"#;
    check_refused(fixture, |db, refused, text| {
        let ComptimeErrorKind::Foreign { func } = refused.kind else {
            panic!("not a foreign call: {text}")
        };
        assert_eq!(FunctionSignature::of(db, func).name.as_str(), "write");
        assert!(text.contains("`write`"), "{text}");
    });
    // `memcpy` and the rest of what the evaluator answers from its memory alone are allowed.
    check_number(
        r#"
//- minicore: sized
#![feature(blaze_comptime)]
extern "C" {
    fn strlen(s: *const u8) -> usize;
}
const GOAL: usize = const static {
    let text = [b'a', b'b', 0];
    unsafe { strlen(&text as *const [u8; 3] as *const u8) }
};
"#,
        2,
    );
}

/// A `const static` block may not read a pointer's address as an integer, by a cast or a
/// transmute; an integer cast to a pointer and back has no address, and is kept.
#[test]
fn a_const_static_block_may_not_read_an_address() {
    let fixture = |goal: &str| {
        format!(
            "//- minicore: sized, transmute\n#![feature(blaze_comptime)]\n\
             const GOAL: usize = const static {{ {goal} }};\n"
        )
    };
    let how = |expected: AddressRead| {
        move |_: &TestDB, refused: &ComptimeError<'_>, text: &str| {
            assert_eq!(
                refused.kind,
                ComptimeErrorKind::PointerAddress { how: expected },
                "{text}"
            );
        }
    };
    check_refused(&fixture("let x = 1u8; &x as *const u8 as usize"), how(AddressRead::Cast));
    check_refused(
        &fixture("let x = 1u8; unsafe { core::mem::transmute::<*const u8, usize>(&x) }"),
        how(AddressRead::Transmute),
    );
    check_number(&fixture("8usize as *const u8 as usize"), 8);
}
