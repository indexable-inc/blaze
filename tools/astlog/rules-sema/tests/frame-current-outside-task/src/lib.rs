//! The shape of `crates/blaze-cargo/src/serve/unit/deps.rs:1712`: `PagedRecord::of` reads
//! `frame::current` for a record it cannot map, and the loop turn that serves dependencies
//! calls it in no `frame::task`, so the daemon panics "frame::current outside any frame::task".

use blaze_mem::frame::{self, Tag};

fn read_held(key: u64) -> usize {
    frame::current(|_frame| key as usize)
}

fn paged_record(key: u64, mappable: bool) -> usize {
    if mappable { 0 } else { read_held(key) }
}

// Found: a root that reaches `frame::current` through two calls and no task.
pub fn serve_deps(keys: &[u64]) -> usize {
    keys.iter().map(|&key| paged_record(key, false)).sum()
}

// Not found: the same path, run inside a task.
pub fn serve_in_task(keys: &[u64]) -> usize {
    frame::task(Tag("serve"), |_frame| keys.iter().map(|&key| paged_record(key, false)).sum())
}

// Not found: a root that takes the frame it needs as an argument.
pub fn with_frame(_frame: &frame::Frame, key: u64) -> usize {
    key as usize
}

// Found: calls `frame::current` itself, in no task.
pub fn direct() -> usize {
    frame::current(|_frame| 1)
}
