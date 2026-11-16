//! twin of flat_drop_glue.rs: the field with drop glue removed, so it compiles.
#![feature(allocator_api)]

use std::{alloc::Global, num::NonZeroUsize};

pub struct Job {
    pub size: u64,
}

pub fn run() {
    let jobs = bstd::sync::chan::bounded::<Job, _>("jobs", NonZeroUsize::MIN, Global);
    let _ = jobs;
}
