//! refused: the type has drop glue
// A channel's message with a `Drop` impl inside, though nothing in it is an address.
#![feature(allocator_api)]

use std::{alloc::Global, num::NonZeroUsize};

pub struct Logged(pub u32);

impl Drop for Logged {
    fn drop(&mut self) {}
}

pub struct Job {
    pub size: u64,
    pub logged: Logged,
}

pub fn run() {
    let jobs = bstd::sync::chan::bounded::<Job, _>("jobs", NonZeroUsize::MIN, Global);
    let _ = jobs;
}
