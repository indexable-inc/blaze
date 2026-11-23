//! The allocator's entry points on the run path (front2/alloc_shim.rs): a call of one of the
//! `extern "Rust"` declarations the pinned alloc writes is a call of its default (std's
//! `__rdl_*`), never a symbol asked of the process. It needs no toolchain: the programs are
//! `#![no_core]`, declare the six as alloc does, and define the defaults as std does.
//!
//! The pinned alloc (library/alloc/src/alloc.rs, nightly-2026-10-27, lines 12-44):
//! ```text
//! unsafe extern "Rust" {
//!     // These are the magic symbols to call the global allocator. rustc generates
//!     // them to call the global allocator if there is a `#[global_allocator]` attribute
//!     // (the code expanding that attribute macro generates those functions), or to call
//!     // the default implementations in std (`__rdl_alloc` etc. in `library/std/src/alloc.rs`)
//!     // otherwise.
//!     fn __rust_alloc(size: usize, align: Alignment) -> *mut u8;
//!     fn __rust_dealloc(ptr: NonNull<u8>, size: usize, align: Alignment);
//!     ..
//!     fn __rust_no_alloc_shim_is_unstable_v2();
//! }
//! ```
//! (each with `#[rustc_std_internal_symbol]`), called from `alloc::alloc::alloc` as
//! `__rust_no_alloc_shim_is_unstable_v2(); __rust_alloc(layout.size(), layout.alignment())`.
//! The pinned std (library/std/src/alloc.rs, `pub mod __default_lib_allocator`, lines
//! 456-510): "These magic symbol names are used as a fallback for implementing the
//! `__rust_alloc` etc symbols (see `src/liballoc/alloc.rs`) when there is no
//! `#[global_allocator]` attribute.":
//! ```text
//! #[rustc_std_internal_symbol]
//! pub unsafe extern "C" fn __rdl_alloc(size: usize, align: usize) -> *mut u8 { .. imp::alloc(layout) }
//! #[rustc_std_internal_symbol]
//! pub unsafe extern "C" fn __rdl_dealloc(ptr: *mut u8, size: usize, align: usize) { .. }
//! ```
//! `Alignment` is `#[repr(transparent)]` over a `#[repr(usize)]` enum (library/core/src/mem/
//! alignment.rs): mirrored here as a transparent newtype of `usize`.
//!
//! The lines these tests print when they pass:
//! ```text
//! ALLOC SHIM: `__rdl_alloc` and `__rdl_dealloc` tabled with their bodies; no foreign symbol `__rust_*`; BUILDS: 3 of 3 instances (0 refused)
//! test a_call_of_rust_alloc_binds_the_default ... ok
//! OWN ALLOCATOR: front2: `..`, reached from `main`, is not checked (..`#[global_allocator]` static..): ..
//! test a_global_allocator_is_refused_by_name ... ok
//! OWN ALLOCATOR SHIM: `__rust_alloc` and `__rust_dealloc` of the `#[global_allocator]` expansion tabled with their bodies, `Counting`'s `alloc` and `dealloc` reached; no `__rdl_*`; BUILDS: N of N instances (0 refused)
//! test a_global_allocator_binds_its_expansion ... ok
//! ```
//! What a wrong fix fails: a declaration left tabled as a foreign fn (the process asked for
//! `__rust_alloc`, today's stop) fails the first test's `no foreign symbol` assertion; one
//! bound but whose default the walk did not queue fails `__rdl_alloc tabled`; a walk that
//! queued it but a call left unbound fails `0 refused`; a marker with no build in place fails
//! `0 refused` too (its call has no body, symbol or intrinsic); a binding to the defaults that
//! ignores a `#[global_allocator]` fails the second test.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front::FnKind;
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};
use blaze_ladder::seam::FrontEnd;

/// The program's start: the lang items a `#![no_core]` program declares, and alloc's
/// declarations as it writes them.
const HEAD: &str = "#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

#[repr(transparent)]
struct Alignment {
    value: usize,
}

unsafe extern \"Rust\" {
    #[rustc_std_internal_symbol]
    fn __rust_alloc(size: usize, align: Alignment) -> *mut u8;
    #[rustc_std_internal_symbol]
    fn __rust_dealloc(ptr: *mut u8, size: usize, align: Alignment);
    #[rustc_std_internal_symbol]
    fn __rust_no_alloc_shim_is_unstable_v2();
}
";

/// The defaults as std writes them, over the C allocator std's `imp` calls on unix.
const DEFAULTS: &str = "
unsafe extern \"C\" {
    fn malloc(size: usize) -> *mut u8;
    fn free(ptr: *mut u8);
}

#[rustc_std_internal_symbol]
pub unsafe extern \"C\" fn __rdl_alloc(size: usize, align: usize) -> *mut u8 {
    unsafe { malloc(size) }
}

#[rustc_std_internal_symbol]
pub unsafe extern \"C\" fn __rdl_dealloc(ptr: *mut u8, size: usize, align: usize) {
    unsafe { free(ptr) }
}
";

/// `Box::new(7u8)` read back, as `alloc::alloc::alloc` and `dealloc` make it.
const MAIN: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    unsafe {
        __rust_no_alloc_shim_is_unstable_v2();
        let p = __rust_alloc(1, Alignment { value: 1 });
        *p = 7;
        let v = *p;
        __rust_dealloc(p, 1, Alignment { value: 1 });
        v as i32
    }
}
";

/// The program's own allocator, as a program writes one (its `GlobalAlloc` impl is not
/// needed: the program is refused at the first allocator call, before the static is read).
const OWN: &str = "
struct Mine;

#[global_allocator]
static MINE: Mine = Mine;
";

/// The program `text` made for a run (`Front2Program::new`): its refusal's text, or `None`
/// when it is made.
fn run_refusal(text: &str) -> Option<String> {
    let files = [SourceFile { path: "src/main.rs", text }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the program's frame");
    match Front2Program::new(&names, &frame, &files, blaze_own::Alloc::global()) {
        Ok(_) => None,
        Err(e) => Some(format!("{e}")),
    }
}

/// The walk from `main` meets the calls of `__rust_alloc` and `__rust_dealloc` and queues
/// their defaults: `__rdl_alloc` and `__rdl_dealloc` are tabled WITH THEIR BODIES, nothing is
/// left out, no row is the symbol of a `__rust_*` declaration (only `malloc` and `free` are
/// foreign), and the back half builds all three bodies with no refusal: each call is an
/// instance of its default, and the marker's call is built in place as nothing.
///
/// Prints: `ALLOC SHIM: `__rdl_alloc` and `__rdl_dealloc` tabled with their bodies; no
/// foreign symbol `__rust_*`; BUILDS: 3 of 3 instances (0 refused)` and
/// `test a_call_of_rust_alloc_binds_the_default ... ok`
#[test]
fn a_call_of_rust_alloc_binds_the_default() {
    let text = [HEAD, DEFAULTS, MAIN].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn the walk reaches; left out: {left:?}");
    let rows: Vec<(String, bool, FnKind, String)> = (0..program.fn_count())
        .map(|i| {
            let f = program.function(i);
            (f.path.to_string(), f.body.is_some(), f.kind, f.symbol.to_string())
        })
        .collect();
    for default in ["__rdl_alloc", "__rdl_dealloc"] {
        let row = rows.iter().find(|(path, ..)| path.ends_with(default));
        assert!(matches!(row, Some((_, true, FnKind::Free, _))), "`{default}` tabled with its body: {rows:?}");
    }
    let shims: Vec<&(String, bool, FnKind, String)> = rows.iter().filter(|(_, _, _, symbol)| symbol.starts_with("__rust_")).collect();
    assert!(shims.is_empty(), "no row is the symbol of a `__rust_*` declaration: {shims:?}");
    let mut foreign: Vec<&str> = rows.iter().filter(|(_, _, kind, _)| *kind == FnKind::Foreign).map(|(_, _, _, symbol)| symbol.as_str()).collect();
    foreign.sort_unstable();
    assert_eq!(foreign, ["free", "malloc"], "only the C allocator is foreign: {rows:?}");
    let with_body = rows.iter().filter(|(_, body, ..)| *body).count();
    assert_eq!(with_body, 3, "`main`, `__rdl_alloc` and `__rdl_dealloc` have bodies: {rows:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the survey stopped: {e}"));
    let refusals: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(refusals.is_empty(), "every body builds, each `__rust_*` call bound or built in place: {refusals:?}");
    assert_eq!((counts.built, counts.tried), (3, 3), "`main`, `__rdl_alloc`, `__rdl_dealloc`");
    println!(
        "ALLOC SHIM: `__rdl_alloc` and `__rdl_dealloc` tabled with their bodies; no foreign symbol `__rust_*`; BUILDS: {} of {} instances ({} refused)",
        counts.built,
        counts.tried,
        refusals.len()
    );
}

/// A program with its own `#[global_allocator]` is refused by the attribute's name and its
/// static's, never bound to std's defaults (which would ignore the program's allocator).
///
/// Prints: `OWN ALLOCATOR: front2: `..`, reached from `main`, is not checked (..`#[global_allocator]`
/// static..): ..` and `test a_global_allocator_is_refused_by_name ... ok`
#[test]
fn a_global_allocator_is_refused_by_name() {
    let refusal = run_refusal(&[HEAD, DEFAULTS, OWN, MAIN].concat()).expect("a program with its own global allocator is refused");
    println!("OWN ALLOCATOR: {refusal}");
    assert!(refusal.contains("`#[global_allocator]` static"), "{refusal}");
    assert!(refusal.contains("MINE"), "the static is named: {refusal}");
    // Without it the same program is made.
    assert_eq!(run_refusal(&[HEAD, DEFAULTS, MAIN].concat()), None);
}

/// A program with its own allocator, as core and alloc give one to a program: the built-in
/// attribute macro as core defines it (`$crate` is this crate, so the expansion's
/// `$crate::alloc::..` and `$crate::mem::..` paths name the two modules here, as in core they
/// name core's), core's `GlobalAlloc` and `Layout` (both of `Layout`'s unchecked constructors,
/// whichever the expansion calls), alloc's declarations, and a counting allocator that records
/// the size it was asked for (a `#![no_core]` program has no `Add`, so the record is the size
/// itself). NO `__rdl_*` is defined: a binding to the defaults is refused for want of one.
const OWN_HEAD: &str = "#![feature(no_core, lang_items, rustc_attrs, decl_macro)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

#[rustc_builtin_macro]
pub macro global_allocator($item:item) {}

pub mod mem {
    #[repr(transparent)]
    pub struct Alignment {
        pub value: usize,
    }
}

pub mod ptr {
    #[repr(transparent)]
    pub struct NonNull<T> {
        pub pointer: *const T,
    }

    impl<T> NonNull<T> {
        pub const fn as_ptr(self) -> *mut T {
            self.pointer as *mut T
        }
    }
}

pub mod alloc {
    pub struct Layout {
        pub size: usize,
        pub align: usize,
    }

    impl Layout {
        pub const unsafe fn from_size_align_unchecked(size: usize, align: usize) -> Layout {
            Layout { size, align }
        }

        pub const unsafe fn from_size_alignment_unchecked(size: usize, alignment: super::mem::Alignment) -> Layout {
            Layout { size, align: alignment.value }
        }
    }

    pub unsafe trait GlobalAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8;
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout);
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8;
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8;
    }
}

unsafe extern \"Rust\" {
    #[rustc_std_internal_symbol]
    fn __rust_alloc(size: usize, align: mem::Alignment) -> *mut u8;
    #[rustc_std_internal_symbol]
    fn __rust_dealloc(ptr: ptr::NonNull<u8>, size: usize, align: mem::Alignment);
    #[rustc_std_internal_symbol]
    fn __rust_no_alloc_shim_is_unstable_v2();
}

unsafe extern \"C\" {
    fn malloc(size: usize) -> *mut u8;
    fn calloc(count: usize, size: usize) -> *mut u8;
    fn realloc(ptr: *mut u8, size: usize) -> *mut u8;
    fn free(ptr: *mut u8);
}

static mut ASKED: usize = 0;

pub struct Counting;

unsafe impl alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: alloc::Layout) -> *mut u8 {
        unsafe {
            ASKED = layout.size;
            malloc(layout.size)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: alloc::Layout) {
        unsafe { free(ptr) }
    }

    unsafe fn alloc_zeroed(&self, layout: alloc::Layout) -> *mut u8 {
        unsafe { calloc(1, layout.size) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, _layout: alloc::Layout, new_size: usize) -> *mut u8 {
        unsafe { realloc(ptr, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;
";

/// `Box::new(1u8)` as `alloc::alloc::alloc` and `dealloc` make it (`exchange_malloc` calls
/// `alloc`, which calls the marker and then `__rust_alloc(layout.size(), layout.alignment())`),
/// answering the size the allocator was asked for: 1 when the call went through `Counting`.
const OWN_MAIN: &str = "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    unsafe {
        __rust_no_alloc_shim_is_unstable_v2();
        let p = __rust_alloc(1, mem::Alignment { value: 1 });
        *p = 1;
        __rust_dealloc(ptr::NonNull { pointer: p as *const u8 }, 1, mem::Alignment { value: 1 });
        ASKED as i32
    }
}
";

/// With a `#[global_allocator]` static, a call of `__rust_alloc` (and `__rust_dealloc`) is a
/// call of the fn of that name the attribute's expansion wrote beside the static, and that
/// fn's body calls `Counting`'s `GlobalAlloc` method: the walk tables both expansion fns WITH
/// BODIES and reaches `Counting`'s `alloc` and `dealloc`, nothing is left out, no foreign row is
/// a `__rust_*` symbol, and every body builds. A binding to std's defaults fails (no `__rdl_*`
/// is defined, so it is refused `NoDefault`), and so does a refusal of the program for having
/// its own allocator (`NotExpanded`, today's stop until the names library expands the
/// attribute: blaze-expand's `BuiltinAttr::GlobalAllocator`).
///
/// Prints: `OWN ALLOCATOR SHIM: `__rust_alloc` and `__rust_dealloc` of the
/// `#[global_allocator]` expansion tabled with their bodies, `Counting`'s `alloc` and `dealloc`
/// reached; no `__rdl_*`; BUILDS: N of N instances (0 refused)` and
/// `test a_global_allocator_binds_its_expansion ... ok`
#[test]
fn a_global_allocator_binds_its_expansion() {
    let text = [OWN_HEAD, OWN_MAIN].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn the walk reaches, the expansion's included; left out: {left:?}");
    let rows: Vec<(String, bool, FnKind, String)> = (0..program.fn_count())
        .map(|i| {
            let f = program.function(i);
            (f.path.to_string(), f.body.is_some(), f.kind, f.symbol.to_string())
        })
        .collect();
    for shim in ["__rust_alloc", "__rust_dealloc"] {
        let written = rows.iter().any(|(path, body, kind, _)| path.ends_with(shim) && *body && *kind == FnKind::Free);
        assert!(written, "`{shim}` of the expansion tabled with its body: {rows:?}");
    }
    // front2 tables every fn with a body as `FnKind::Free` (program/tabled.rs `row`), an impl's
    // member too: the member is told by its path, `<.. for Counting>::alloc`, never by the
    // trait's declaration `GlobalAlloc::alloc` (no body, `Trait`).
    for method in ["::alloc", "::dealloc"] {
        let reached = rows.iter().any(|(path, body, kind, _)| path.ends_with(method) && path.contains("for Counting>") && *body && *kind == FnKind::Free);
        assert!(reached, "`Counting`'s `{method}` reached through the expansion: {rows:?}");
    }
    let defaults: Vec<&String> = rows.iter().map(|(path, ..)| path).filter(|path| path.contains("__rdl_")).collect();
    assert!(defaults.is_empty(), "no default is reached: {defaults:?}");
    let foreign_shims: Vec<&String> = rows.iter().filter(|(_, _, kind, symbol)| *kind == FnKind::Foreign && symbol.starts_with("__rust_")).map(|(_, _, _, symbol)| symbol).collect();
    assert!(foreign_shims.is_empty(), "no `__rust_*` declaration is asked of the process: {foreign_shims:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("the survey stopped: {e}"));
    let refusals: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(refusals.is_empty(), "every body builds, each `__rust_*` call bound to the expansion or built in place: {refusals:?}");
    assert_eq!(counts.built, counts.tried, "every instance built");
    println!(
        "OWN ALLOCATOR SHIM: `__rust_alloc` and `__rust_dealloc` of the `#[global_allocator]` expansion tabled with their bodies, `Counting`'s `alloc` and `dealloc` reached; no `__rdl_*`; BUILDS: {} of {} instances ({} refused)",
        counts.built,
        counts.tried,
        refusals.len()
    );
}
