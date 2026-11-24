//! The entry line and the hosted call (run.md, "Every program runs in the child"): the parent's
//! `BLAZE_LAUNCH_ENTRY` reads `hosted:<symbol>` as bstd's hosted entry and still refuses an
//! unknown form by name; and a hosted entry is called with the host's table, its `i32` the
//! status the process exits with.

use core::ffi::{c_int, c_void};
use core::ptr::NonNull;

use blaze_code::record::Symbol as CodeSymbol;

use super::{EntryExit, EntryForm, Handed, HostTableAt, Start, call_entry};

const HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF";

fn symbol() -> CodeSymbol {
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (index as u8 % 16) * 0x11;
    }
    CodeSymbol(bytes)
}

/// `hosted:<64 hex>` reads as [`EntryForm::Hosted`] with the symbol's bytes (either case of
/// hex), beside the two forms there were; before this piece it was refused with "a hosted entry
/// needs bstd's host table", which this assertion fails on.
#[test]
fn a_hosted_entry_line_reads_as_the_hosted_form() {
    let line = format!("hosted:{HEX}");
    assert_eq!(Start::parse(line.as_bytes()), Ok(Start { entry: symbol(), form: EntryForm::Hosted }));
    assert_eq!(Start::parse(format!("unit:{HEX}").as_bytes()).map(|it| it.form), Ok(EntryForm::Unit));
    assert_eq!(Start::parse(format!("c-main:{HEX}").as_bytes()).map(|it| it.form), Ok(EntryForm::CMain));
}

/// `comptime:<64 hex>` (D175) reads as [`EntryForm::Comptime`] with the symbol's bytes; a
/// compile-time line with a short symbol is refused as any other form's is. Before this piece it
/// was refused as a form none of the three.
#[test]
fn a_comptime_entry_line_reads_as_the_comptime_form() {
    let line = format!("comptime:{HEX}");
    assert_eq!(Start::parse(line.as_bytes()), Ok(Start { entry: symbol(), form: EntryForm::Comptime }));
    assert_eq!(Start::parse(b"comptime:abcd").map_err(|why| why.why), Err("the symbol is not 64 hex digits"));
}

/// An unknown form is refused, naming the four there are; a hosted line with a short symbol or
/// a byte that is no hex digit is refused as any other form's is.
#[test]
fn an_unknown_form_or_a_bad_symbol_is_refused_by_name() {
    let refused = |line: &str| Start::parse(line.as_bytes()).map_err(|why| why.why);
    let none = "the form is none of `unit`, `c-main`, `hosted` and `comptime`";
    assert_eq!(refused(&format!("lib:{HEX}")), Err(none));
    assert_eq!(refused(&format!("Hosted:{HEX}")), Err(none));
    assert_eq!(refused(&format!("comptime-{HEX}")), Err(none));
    assert_eq!(refused("hosted:abcd"), Err("the symbol is not 64 hex digits"));
    assert_eq!(refused(&format!("hosted:{}", "g".repeat(64))), Err("the symbol holds a character that is no hex digit"));
    let said = Start::parse(b"lib:00").expect_err("an unknown form").to_string();
    assert!(said.contains("`unit`, `c-main`, `hosted` or `comptime`"), "{said}");
}

/// A compile-time entry is called as C `main` with the host's arguments, not the process's:
/// handed two arguments it answers 2; the control, handed none, answers 0.
#[test]
fn a_comptime_entry_is_called_with_the_hosts_arguments() {
    use super::ComptimeArgs;
    extern "C" fn count(argc: c_int, argv: *const *const core::ffi::c_char) -> c_int {
        // SAFETY: `argv` holds `argc` strings and a null (the test's words below).
        if unsafe { !(*argv.add(argc as usize)).is_null() } { -1 } else { argc }
    }
    let words: [*const core::ffi::c_char; 3] = [c"macro".as_ptr(), c"--derive".as_ptr(), core::ptr::null()];
    // SAFETY: two C-string literals (static) and a null, alive across the call.
    let two = unsafe { ComptimeArgs::new(2, words.as_ptr()) };
    assert_eq!(call_entry(count as usize as u64, Handed::Comptime(two)), EntryExit::Returned(2));
    let empty: [*const core::ffi::c_char; 1] = [core::ptr::null()];
    // SAFETY: as above.
    let none = unsafe { ComptimeArgs::new(0, empty.as_ptr()) };
    assert_eq!(call_entry(count as usize as u64, Handed::Comptime(none)), EntryExit::Returned(0));
}

/// A stand-in for bstd's host table: only its address is compared, never read.
static TABLE: [u64; 4] = [0; 4];

/// A hosted entry of this form: 7 when handed [`TABLE`], 1 when handed anything else.
extern "C" fn hosted_seven(table: *const c_void) -> c_int {
    if core::ptr::eq(table, core::ptr::from_ref(&TABLE).cast()) { 7 } else { 1 }
}

/// A hosted entry is called as `extern "C" fn(*const HostTable) -> i32` with the table the
/// launcher made, and what it returns is the status `run` hands `exit` (7 here). Control: handed
/// another table, the same entry answers 1, so the 7 is the table's arriving, not a constant.
#[test]
fn a_hosted_entry_returning_seven_ends_with_seven() {
    // SAFETY: the stand-in is never read (`hosted_seven` compares its address only), and the
    // static outlives the call.
    let table = unsafe { HostTableAt::new(NonNull::from(&TABLE).cast::<c_void>()) };
    assert_eq!(call_entry(hosted_seven as usize as u64, Handed::Hosted(table)), EntryExit::Returned(7));
    static OTHER: [u64; 4] = [0; 4];
    // SAFETY: as above.
    let other = unsafe { HostTableAt::new(NonNull::from(&OTHER).cast::<c_void>()) };
    assert_eq!(call_entry(hosted_seven as usize as u64, Handed::Hosted(other)), EntryExit::Returned(1));
}

/// An uncaught guest panic raised by the guest's `panic_unwind` carries `MOZ\0RUST` with the
/// guest's own `CANARY` address (different from the launcher host's `CANARY`). `call_entry`
/// catches it at the host/guest boundary via `core::intrinsics::catch_unwind`: guest
/// destructors run in phase 2, `exception_cleanup` is not invoked, and host
/// `panic_unwind::__rust_panic_cleanup` is never reached (before this fix, letting the
/// exception unwind into `std::thread::scope` / `libtest` aborted with
/// `"fatal runtime error: Rust cannot catch foreign exceptions, aborting"`).
#[test]
fn a_guest_unwind_with_foreign_canary_is_caught_at_the_entry_boundary() {
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, Ordering};

    const RUST_EXCEPTION_CLASS: u64 = u64::from_ne_bytes(*b"MOZ\0RUST");
    static GUEST_CANARY: u8 = 0;
    static DROPPED: AtomicBool = AtomicBool::new(false);
    static CLEANUP_CALLED: AtomicBool = AtomicBool::new(false);

    #[repr(C, align(16))]
    struct ForeignRustException {
        exception_class: u64,
        exception_cleanup: Option<extern "C" fn(c_int, *mut ForeignRustException)>,
        private: [*const u8; 2],
        canary: *const u8,
    }

    struct SyncExc(UnsafeCell<ForeignRustException>);
    // SAFETY: accessed only on this single test thread while raising the test exception.
    unsafe impl Sync for SyncExc {}

    static EXC: SyncExc = SyncExc(UnsafeCell::new(ForeignRustException {
        exception_class: RUST_EXCEPTION_CLASS,
        exception_cleanup: Some(on_cleanup),
        private: [core::ptr::null(); 2],
        canary: &raw const GUEST_CANARY,
    }));

    extern "C" fn on_cleanup(_code: c_int, _exc: *mut ForeignRustException) {
        CLEANUP_CALLED.store(true, Ordering::Relaxed);
    }

    unsafe extern "C-unwind" {
        fn _Unwind_RaiseException(exception: *mut ForeignRustException) -> c_int;
    }

    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            DROPPED.store(true, Ordering::Relaxed);
        }
    }

    extern "C-unwind" fn guest_unwinds() {
        let _guard = Guard;
        // SAFETY: `EXC` is 16-byte aligned, lives past this frame, and matches the Itanium
        // `_Unwind_Exception` layout plus `panic_unwind`'s `canary` field.
        unsafe {
            let exc = EXC.0.get();
            (*exc).private = [core::ptr::null(); 2];
            _Unwind_RaiseException(exc);
        }
    }

    extern "C-unwind" fn guest_returns() {}

    DROPPED.store(false, Ordering::Relaxed);
    CLEANUP_CALLED.store(false, Ordering::Relaxed);

    assert_eq!(call_entry(guest_returns as *const () as usize as u64, Handed::Unit), EntryExit::Returned(0));
    assert_eq!(call_entry(guest_unwinds as *const () as usize as u64, Handed::Unit), EntryExit::Unwound);
    assert!(DROPPED.load(Ordering::Relaxed), "phase 2 unwinding must run guest destructors before stopping at call_entry");
    assert!(!CLEANUP_CALLED.load(Ordering::Relaxed), "call_entry must not invoke _Unwind_DeleteException on a guest panic");
}
