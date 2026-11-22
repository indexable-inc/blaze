//! A thread's warm frames sized by its machine (frame.rs `ShelfSize`, `size_shelf`): eight
//! threads keep what they kept, 128 keep at most 512 MiB idle where they kept 4 GiB, and the
//! process's pool of spans stays under its bound. Each case on threads of its own, whose shelves
//! start empty.

use std::thread;

use super::{CHUNK, SHELF, SPAN_POOL, SPAN_WARM, SPANS, ShelfSize, WARM, WARM_ALL, pooled_span_bytes, size_shelf, spare_bytes, task, trim_spare};
use crate::Tag;

#[test]
fn a_shelf_is_sized_by_its_machine() {
    assert_eq!(ShelfSize::MOST.bytes(), WARM);
    assert_eq!(ShelfSize::for_threads(0), ShelfSize::MOST);
    assert_eq!(ShelfSize::for_threads(1), ShelfSize::MOST);
    assert_eq!(ShelfSize::for_threads(8), ShelfSize::MOST, "the 8-thread figure is today's");
    let wide = ShelfSize::for_threads(128);
    assert_eq!(wide, ShelfSize { chunks: 1, spans: 1 });
    assert!(128 * wide.bytes() <= 512 << 20, "128 threads keep {} bytes warm at most", 128 * wide.bytes());
    for threads in 1..=512 {
        let size = ShelfSize::for_threads(threads);
        assert!(size.chunks >= 1 && size.spans >= 1 && size.chunks <= SHELF && size.spans <= SPANS);
        assert!(ShelfSize::for_threads(threads + 1).bytes() <= size.bytes(), "more threads never keep more each");
        if (8..=64).contains(&threads) {
            assert!(threads * size.bytes() <= WARM_ALL, "{threads} threads share the machine's budget");
        }
    }
}

/// A task of 64 allocations of 100 KiB, in chunks: about four chunks' worth, given back as it
/// ends; then what its thread keeps warm, trimmed after.
fn busy_then_spare() -> usize {
    for _ in 0..2 {
        task(Tag::Codegen, |frame| {
            for _ in 0..64 {
                frame.alloc_slice_fill_with(100 << 10, |i| i as u8);
            }
        });
    }
    let spare = spare_bytes();
    trim_spare(0);
    spare
}

/// Sixteen threads stand for a 128-thread machine's: each sized as one of its workers keeps at
/// most one chunk and one span warm, so the machine's idle frames are under 512 MiB. The cause,
/// tested by removing it: a thread handed no size (today's shelf) keeps more than that bound
/// after the same work, and one sized for eight threads keeps exactly what today's does.
#[test]
fn a_128_thread_machine_keeps_its_idle_frames_under_its_bound() {
    let wide = ShelfSize::for_threads(128);
    let most = (0..16)
        .map(|_| {
            thread::spawn(move || {
                size_shelf(wide);
                busy_then_spare()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().expect("a worker"))
        .max()
        .unwrap_or(0);
    assert!(most <= wide.bytes(), "a 128-thread machine's worker keeps {most} bytes warm, past {}", wide.bytes());
    assert!(128 * most <= 512 << 20);
    let today = thread::spawn(busy_then_spare).join().expect("the control's thread");
    assert!(today > wide.bytes(), "the control: an unsized shelf keeps {today} bytes, no more than a sized one's {}", wide.bytes());
    assert!(today <= WARM);
    let eight = thread::spawn(|| {
        size_shelf(ShelfSize::for_threads(8));
        busy_then_spare()
    })
    .join()
    .expect("the eight-thread machine's worker");
    assert_eq!(eight, today, "a thread of an eight-thread machine keeps what it kept");
    assert!(today >= 3 * CHUNK, "the work leaves chunks warm: {today}");
}

/// Spans a shelf sized for 128 threads has no room for go to the process's pool, which keeps at
/// most [`SPAN_POOL`] of them, each down to [`SPAN_WARM`]; a thread that needs one takes it.
#[test]
fn spans_past_a_shelf_go_to_the_bounded_pool() {
    fn nested(depth: usize) {
        if depth == 0 {
            return;
        }
        task(Tag::Codegen, |frame| {
            let big = frame.alloc_slice_fill_with(3 << 20, |i| i as u8);
            assert_eq!(big[(3 << 20) - 1], ((3 << 20) - 1) as u8);
            nested(depth - 1);
        });
    }
    thread::spawn(|| {
        size_shelf(ShelfSize::for_threads(128));
        for _ in 0..3 {
            nested(SPAN_POOL + 4);
            assert!(spare_bytes() <= ShelfSize::for_threads(128).bytes());
            assert!(pooled_span_bytes() <= SPAN_POOL * SPAN_WARM, "the pool keeps {} bytes", pooled_span_bytes());
        }
        trim_spare(0);
    })
    .join()
    .expect("the spans' thread");
}
