//! The POSIX surface blaze uses on the Unix systems, declared here so that no crate of blaze
//! names the `libc` crate (D180): the `#[repr(C)]` types, the constants per system and
//! architecture, and the calls (signals, file descriptors, sockets, processes, threads).
//!
//! On Linux every item is the kernel's own: a raw system call through [`crate::linux`]'s
//! `syscall`/`svc` wrapper, the error in the return value (no `errno` thread-local), and no
//! C-library symbol referenced at all. What has no kernel form (`dlsym`, `pthread_key_*`,
//! `getpwuid_r`, `localtime_r`) is not offered on Linux; the caller uses the kernel form, the
//! auxiliary vector, `/proc`, or the value it is handed.
//!
//! On macOS the floor is libSystem: Apple keeps no stable system call numbers and requires
//! every call to go through libSystem, which is always dynamically linked. blaze-sys declares
//! libSystem's functions itself, and the set it imports is the ratchet
//! `tools/ratchets/blaze-sys-libsystem.txt`, so a new import is a visible decision.
//!
//! Each item says what it replaces (`/// replaces libc::sigaction`), so a port greps
//! `replaces libc::<name>`. A `#[repr(C)]` type carries a const assertion of its size,
//! alignment and the offsets the tree reads; `tests` in each area compares them, and every
//! constant, with the `libc` crate's own, once, and goes with that dev-dependency.

pub mod io;
pub mod net;
pub mod proc;
pub mod signal;
pub mod thread;
pub mod types;
