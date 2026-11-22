//! The binder seam's composable strategies (run_mode/bind.rs, rules 76 & 78):
//! - `Dlsym` vs `Table` agree on `Own` rows (`memcpy`, `malloc`), while `Table` walls off
//!   unlisted rows (`getpriority`) and never claims launcher intercepts.
//! - `LauncherOwn<Dlsym>` intercepts `pthread_create` (thread/sampler registration),
//!   `_Unwind_RaiseException` (AArch64 span-unwind restoration), and `__dso_handle`, while
//!   delegating ordinary libc symbols identically to `Dlsym`.
//! - `ObservedBind<LauncherOwn<Dlsym>>` intercepts `malloc`/`free`/`read`/`write`/etc. into
//!   zero-touch telemetry wrappers that emit exact events into `crate::telemetry` and return
//!   identical results to unintercepted calls.

use core::ffi::{CStr, c_void};

use blaze_comptime_sys::Os;

use super::bind::{Bind, BindError, Dlsym, LauncherOwn, ObservedBind, Table};
use crate::telemetry::{AllocOp, EventKind, Telemetry, clear_global, install_global};

/// The system the test process runs on: the table is asked for the host's own rows.
const HOST: Os = if cfg!(target_os = "macos") { Os::Mac } else { Os::Linux };

fn both(name: &str, cname: &CStr) -> (Result<u64, BindError>, Result<u64, BindError>) {
    (Dlsym.address(name, cname), Table::no_data(HOST).address(name, cname))
}

/// Prints "bind: Own rows equal under both; a row-less name refused by the table only; an
/// undefined name refused by both".
#[test]
fn the_two_strategies_agree_on_own_rows_and_the_table_walls_the_rest() {
    for (name, cname) in [("memcpy", c"memcpy"), ("malloc", c"malloc")] {
        let (plain, table) = both(name, cname);
        assert!(plain.is_ok(), "this process defines `{name}`");
        assert_eq!(plain, table, "an `Own` row binds where an ordinary run binds `{name}`");
    }
    // `getpriority` is in every C library and in no row of the table: a compile-time program
    // reaches only what the table answers.
    let (plain, table) = both("getpriority", c"getpriority");
    assert!(plain.is_ok(), "the control: this process defines `getpriority`");
    assert_eq!(table, Err(BindError::NotInTable), "the table walls `getpriority`: {table:?}");
    let (plain, table) = both("blaze_no_such_c_name", c"blaze_no_such_c_name");
    assert_eq!(plain, Err(BindError::NotFound));
    assert!(table.is_err(), "nor does the table bind it");
    // Comptime Table never claims launcher intercepts (isolation by type).
    assert_eq!(Table::no_data(HOST).intercept("pthread_create"), None);
    assert_eq!(Table::no_data(HOST).intercept("malloc"), None);
    println!("bind: Own rows equal under both; a row-less name refused by the table only; an undefined name refused by both");
}

#[test]
fn launcher_own_intercepts_threads_and_unwind_without_touching_ordinary_symbols() {
    let mut own = LauncherOwn::new(Dlsym);
    assert_eq!(
        own.intercept("pthread_create"),
        Some(crate::threads::create as *const () as u64),
        "user-spawned threads must go through threads::create"
    );
    #[cfg(target_arch = "aarch64")]
    assert_eq!(
        own.intercept("_Unwind_RaiseException"),
        Some(crate::observe::unwind_raise_exception as *const () as u64),
        "panics must restore span_exit frames before libunwind walks"
    );
    assert!(own.intercept("__dso_handle").is_some());
    assert_eq!(own.intercept("memcpy"), None);
    assert_eq!(own.intercept("malloc"), None);
    assert_eq!(own.address("memcpy", c"memcpy"), Dlsym.address("memcpy", c"memcpy"));
}

#[test]
fn observed_bind_traces_malloc_and_free_without_user_instrumentation() {
    let _guard = crate::telemetry::TEST_GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev_stride = crate::telemetry::alloc_sample_bytes();
    crate::telemetry::set_alloc_sample_bytes(0);

    let cap = 64u32;
    let bytes = Telemetry::bytes_for(cap).expect("bytes");
    let layout = core::alloc::Layout::from_size_align(bytes, 128).expect("layout");
    let raw = unsafe { std::alloc::alloc_zeroed(layout) };
    let base = core::ptr::NonNull::new(raw).expect("nonnull");
    let ring = unsafe { Telemetry::init(base, bytes, cap) }.expect("init");
    install_global(base, bytes).expect("install");

    let mut obs = ObservedBind::new(LauncherOwn::new(Dlsym));
    // LauncherOwn intercepts still win for pthread_create.
    assert_eq!(obs.intercept("pthread_create"), Some(crate::threads::create as *const () as u64));
    let malloc_addr = obs.intercept("malloc").expect("malloc intercepted");
    let free_addr = obs.intercept("free").expect("free intercepted");

    let my_malloc: extern "C" fn(usize) -> *mut c_void = unsafe { core::mem::transmute(malloc_addr as usize) };
    let my_free: unsafe extern "C" fn(*mut c_void) = unsafe { core::mem::transmute(free_addr as usize) };

    let ptr = my_malloc(128);
    assert!(!ptr.is_null());
    unsafe { my_free(ptr) };

    let mut events = Vec::new();
    ring.drain(16, |e| events.push(e));
    assert_eq!(events.len(), 2, "malloc + free must emit 2 telemetry events: {events:?}");
    assert_eq!(events[0].event_kind(), Some(EventKind::Alloc));
    assert_eq!(events[0].slot_or_op, AllocOp::Malloc as u32);
    assert_eq!(events[0].a, ptr as usize as u64);
    assert_eq!(events[0].b, 128);
    assert_eq!(events[1].event_kind(), Some(EventKind::Free));
    assert_eq!(events[1].slot_or_op, AllocOp::Free as u32);
    assert_eq!(events[1].a, ptr as usize as u64);

    crate::telemetry::set_alloc_sample_bytes(prev_stride);
    clear_global();
    unsafe { std::alloc::dealloc(raw, layout) };
}
