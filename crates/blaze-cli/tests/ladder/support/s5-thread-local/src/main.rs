// Support program s5: thread-locals (crates/blaze-build/src/tls.rs). A second thread adds 100
// to its own copy of `N`; `main` then adds 2 to its copy, which still starts at 40. Prints
// `tls`, exits 42.
#![feature(no_core, lang_items, thread_local)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for u64 {}

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
    fn pthread_create(thread: *mut usize, attr: *const u8, start: extern "C" fn(*mut u8) -> *mut u8, arg: *mut u8) -> i32;
    fn pthread_join(thread: usize, ret: *mut *mut u8) -> i32;
}

#[thread_local]
static mut N: u64 = 40;

extern "C" fn worker(_: *mut u8) -> *mut u8 {
    unsafe {
        N = N + 100;
    }
    0 as *mut u8
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    unsafe {
        let mut t: usize = 0;
        if pthread_create(&mut t, 0 as *const u8, worker, 0 as *mut u8) != 0 {
            return 1;
        }
        pthread_join(t, 0 as *mut *mut u8);
        N = N + 2;
        write(1, b"tls\n" as *const u8, 4);
        N as i32
    }
}
