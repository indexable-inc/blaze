//! C's allocations are blaze's (`src/malloc.rs`): a block C makes while a task runs is counted
//! to the task's tag, a block another allocator made frees through it, and an LLVM context made
//! and disposed of in a task leaves the task's tag holding what it held before.
//!
//! An integration test, so this binary's global allocator and C allocator are the library's own
//! (`lib.rs`, built without `cfg(test)`), as the daemon's are.
#![cfg(all(feature = "native", any(target_os = "linux", target_os = "macos")))]
#![allow(unsafe_code, reason = "C's allocator and LLVM's C API, called as C calls them")]

use std::ffi::{CString, c_char, c_void};

use blaze_mem::Tag;
use llvm_sys::core::{
    LLVMAddFunction, LLVMAppendBasicBlockInContext, LLVMBuildAdd, LLVMBuildRet, LLVMContextCreate,
    LLVMContextDispose, LLVMCreateBuilderInContext, LLVMDisposeBuilder, LLVMDisposeModule,
    LLVMFunctionType, LLVMGetParam, LLVMInt64TypeInContext, LLVMModuleCreateWithNameInContext,
    LLVMPositionBuilderAtEnd,
};

unsafe extern "C" {
    fn strdup(text: *const c_char) -> *mut c_char;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// What `tag`'s blocks in the global heap hold now: exact for this thread's own, which is all
/// this binary allocates in the tags these tests use.
fn held(tag: Tag) -> i128 {
    blaze_graph::arena::fold_here();
    i128::from(blaze_mem::tag::snapshot().get(tag).heap)
}

fn installed() {
    blaze_engine::malloc::check().expect("blaze's C allocator is this process's");
}

/// A block libc's `strdup` makes in a task (C code, calling `malloc` as C does) is counted to
/// the task's tag at no less than it asked for, grown by `realloc` it is counted at its new
/// size, and freed it is taken off again, to the byte.
#[test]
fn a_block_c_makes_in_a_task_is_counted_to_the_tasks_tag() {
    installed();
    const BYTES: usize = 1 << 20;
    const GROWN: usize = 4 << 20;
    // alloc: the text C copies, made outside the task
    let text = CString::new(vec![b'x'; BYTES]).expect("no NUL in the text");
    // The first round warms what the task and the readings make once.
    let mut seen = (0, 0, 0, 0);
    for _ in 0..2 {
        seen = blaze_mem::frame::task(Tag::Fuzz, |_| {
            let before = held(Tag::Fuzz);
            // SAFETY: a NUL-terminated string.
            let copy = unsafe { strdup(text.as_ptr()) };
            assert!(!copy.is_null(), "strdup made a block");
            let copied = held(Tag::Fuzz);
            // SAFETY: the block strdup made, not used after.
            let grown = unsafe { realloc(copy.cast(), GROWN) };
            assert!(!grown.is_null(), "realloc grew it");
            let regrown = held(Tag::Fuzz);
            // SAFETY: the block realloc gave, freed once.
            unsafe { free(grown) };
            (before, copied, regrown, held(Tag::Fuzz))
        });
    }
    let (before, copied, grown, after) = seen;
    let slack = 64 << 10;
    assert!(
        (BYTES as i128 + 1..BYTES as i128 + 1 + slack).contains(&(copied - before)),
        "a {} B copy moved the fuzz tag's heap by {} B",
        BYTES + 1,
        copied - before,
    );
    assert!(
        (GROWN as i128..GROWN as i128 + slack).contains(&(grown - before)),
        "grown to {GROWN} B it moved the tag by {} B",
        grown - before,
    );
    assert_eq!(after, before, "freed, the tag holds what it held before");
}

/// A block the zone that was first before blaze's made (as one made before the constructor
/// ran) frees through C's `free`, grows through `realloc`, and frees through the default zone,
/// which hands every call to the first zone, blaze's, which hands it to its owner.
#[cfg(target_os = "macos")]
#[test]
fn a_block_made_before_blazes_zone_frees_through_it() {
    use std::ffi::CStr;

    unsafe extern "C" {
        fn malloc_get_all_zones(
            task: u32,
            reader: *const c_void,
            addresses: *mut *mut usize,
            count: *mut u32,
        ) -> i32;
        fn malloc_get_zone_name(zone: *mut c_void) -> *const c_char;
        fn malloc_zone_malloc(zone: *mut c_void, size: usize) -> *mut c_void;
        fn malloc_zone_free(zone: *mut c_void, ptr: *mut c_void);
        fn malloc_default_zone() -> *mut c_void;
        static mach_task_self_: u32;
    }
    installed();
    let mut zones: *mut usize = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: this task's zones, read in place; both out-pointers ours.
    let listed =
        unsafe { malloc_get_all_zones(mach_task_self_, std::ptr::null(), &raw mut zones, &raw mut count) };
    assert_eq!(listed, 0, "libmalloc lists its zones");
    // SAFETY: `count` zones.
    let zones = unsafe { std::slice::from_raw_parts(zones, count as usize) };
    let name = |zone: usize| {
        // SAFETY: a registered zone; its name lives as long as it does.
        unsafe { CStr::from_ptr(malloc_get_zone_name(std::ptr::with_exposed_provenance_mut(zone))) }
    };
    assert_eq!(name(zones[0]), c"blaze", "blaze's zone is first");
    let before = std::ptr::with_exposed_provenance_mut::<c_void>(
        *zones.iter().skip(1).find(|&&zone| name(zone) != c"blaze").expect("the system's zone is still registered"),
    );
    // SAFETY: each block is the other zone's, live, and freed once.
    unsafe {
        let small = malloc_zone_malloc(before, 100);
        assert!(!small.is_null());
        free(small);
        let grown = realloc(malloc_zone_malloc(before, 100), 1 << 20);
        assert!(!grown.is_null());
        free(grown);
        malloc_zone_free(malloc_default_zone(), malloc_zone_malloc(before, 100));
    }
}

/// A block glibc's own allocator made (as one made by a library that calls it by its own name)
/// frees through C's `free` and grows through `realloc`: each goes to glibc.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn a_block_glibc_made_frees_through_blazes_free() {
    unsafe extern "C" {
        fn __libc_malloc(size: usize) -> *mut c_void;
    }
    installed();
    // SAFETY: each block is glibc's, live, and freed once.
    unsafe {
        let small = __libc_malloc(100);
        assert!(!small.is_null());
        free(small);
        let grown = realloc(__libc_malloc(100), 1 << 20);
        assert!(!grown.is_null());
        free(grown);
    }
}

/// Makes an LLVM context and a module of 64 small functions through LLVM's C API, reads
/// `alive` while they live, and disposes of both.
fn in_llvm(alive: impl FnOnce() -> i128) -> i128 {
    // SAFETY: LLVM's C API as vendor/llvm-sys's contract says: the builder and the module are
    // disposed of before their context, every handle used only while its owner lives, every
    // name NUL-terminated.
    unsafe {
        let context = LLVMContextCreate();
        let module = LLVMModuleCreateWithNameInContext(c"blaze".as_ptr(), context);
        let int = LLVMInt64TypeInContext(context);
        let mut params = [int, int];
        let ty = LLVMFunctionType(int, params.as_mut_ptr(), 2, 0);
        for _ in 0..64 {
            // LLVM names each one apart (add, add.1, ...).
            let function = LLVMAddFunction(module, c"add".as_ptr(), ty);
            let entry = LLVMAppendBasicBlockInContext(context, function, c"entry".as_ptr());
            let builder = LLVMCreateBuilderInContext(context);
            LLVMPositionBuilderAtEnd(builder, entry);
            let sum = LLVMBuildAdd(
                builder,
                LLVMGetParam(function, 0),
                LLVMGetParam(function, 1),
                c"sum".as_ptr(),
            );
            LLVMBuildRet(builder, sum);
            LLVMDisposeBuilder(builder);
        }
        let read = alive();
        LLVMDisposeModule(module);
        LLVMContextDispose(context);
        read
    }
}

/// LLVM's allocations (C++'s `operator new`, on `malloc`) in a task are counted to its tag,
/// and an LLVM context made and disposed of there leaves the tag holding what it held before:
/// every byte LLVM freed was taken off the tag that was charged it.
#[test]
fn an_llvm_context_made_and_disposed_in_a_task_leaves_its_tag_where_it_began() {
    installed();
    // Once outside any task: what LLVM makes once for the process (its options, its statics)
    // is made here, and lives on.
    in_llvm(|| 0);
    let mut seen = (0, 0, 0);
    // The first round warms what the task and the readings make once.
    for _ in 0..2 {
        seen = blaze_mem::frame::task(Tag::Codegen, |_| {
            let before = held(Tag::Codegen);
            let alive = in_llvm(|| held(Tag::Codegen));
            (before, alive, held(Tag::Codegen))
        });
    }
    let (before, alive, after) = seen;
    assert!(
        alive - before >= 4 << 10,
        "a live LLVM context and module moved the codegen tag's heap by only {} B",
        alive - before,
    );
    assert_eq!(after, before, "disposed of, LLVM left the codegen tag holding {} B more", after - before);
}
