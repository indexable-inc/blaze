//! The macOS direct read the thread-local access sequence rests on (`pass/tls_index.rs`; the
//! sequence itself is run natively by blaze-build's `tls/index_tests.rs`).

#![cfg(all(target_arch = "aarch64", target_os = "macos"))]

unsafe extern "C" {
    fn pthread_key_create(key: *mut usize, destructor: Option<unsafe extern "C" fn(*mut u8)>) -> i32;
    fn pthread_getspecific(key: usize) -> *mut u8;
    fn pthread_setspecific(key: usize, value: *const u8) -> i32;
}

/// The thread's specific-data slot `key` read as `_pthread_getspecific_direct` does: the
/// slots at `tpidrro_el0 & ~7`, one word per key.
fn direct(key: usize) -> usize {
    let base: usize;
    // SAFETY: reading `tpidrro_el0` has no effect; user code may read it on macOS.
    unsafe { core::arch::asm!("mrs {}, tpidrro_el0", out(reg) base) };
    let slots = (base & !7) as *const usize;
    // SAFETY: libpthread keeps the thread's slots there for the thread's life, a key indexing
    // its own word (the private ABI this test pins).
    unsafe { slots.add(key).read() }
}

/// macOS private ABI (rule 45): the direct read `tls_addr` emits equals `pthread_getspecific`,
/// on two threads with different values.
#[test]
fn the_direct_tsd_read_equals_pthread_getspecific() {
    let mut key = 0usize;
    // SAFETY: `key` is a live out-parameter.
    assert_eq!(unsafe { pthread_key_create(&mut key, None) }, 0);
    let check = |mark: usize| {
        assert_eq!(direct(key), 0, "a fresh thread's slot is empty");
        // SAFETY: the key was created above; the value is never dereferenced.
        assert_eq!(unsafe { pthread_setspecific(key, mark as *const u8) }, 0);
        // SAFETY: as above.
        let official = unsafe { pthread_getspecific(key) } as usize;
        assert_eq!(official, mark);
        assert_eq!(direct(key), official, "the direct read of key {key}");
    };
    std::thread::scope(|s| {
        s.spawn(|| check(0x1111_0000));
        s.spawn(|| check(0x2222_0000));
    });
}
