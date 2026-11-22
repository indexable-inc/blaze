//! refused: is an address of this machine
// A channel's message with a reference inside (`chan::bounded`'s `Flat` assertion).
#![feature(allocator_api)]

use std::{alloc::Global, num::NonZeroUsize};

pub struct Job {
    pub name: &'static str,
    pub size: u64,
}

pub fn run() {
    let jobs = bstd::sync::chan::bounded::<Job, _>("jobs", NonZeroUsize::MIN, Global);
    let _ = jobs;
}
