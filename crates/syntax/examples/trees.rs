//! `cargo run --release -p syntax --example trees -- DIR...`: every `.rs` file under the
//! directories parsed in full and as an outline (its `fn` bodies lazy), on one thread: the time
//! to parse, the bytes all the trees hold at once besides the texts they share, and the time to
//! walk every node and token of each through the cursor's API, as lowering does (which parses
//! an outline's bodies), the bytes held then, then every node alone, as an ast id map does, and
//! one node in 16 found again from a pointer, as a diagnostic's span is.

#![deny(unsafe_code)]

use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use syntax::{AstNode, Edition, SourceFile, SyntaxNodePtr, WalkEvent};

use self::counting::{Counting, HELD};

/// The bytes held, counted by an allocator over the system's: implementing `GlobalAlloc` is
/// unsafe, and only this module does.
#[allow(unsafe_code)]
mod counting {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        sync::atomic::{AtomicIsize, Ordering},
    };

    pub(super) struct Counting;

    pub(super) static HELD: AtomicIsize = AtomicIsize::new(0);

    // SAFETY: every call is passed on to the system's allocator, which is what holds the memory.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            HELD.fetch_add(layout.size() as isize, Ordering::Relaxed);
            // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract (a non-zero-sized
            // `layout`), which is `System`'s too.
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            HELD.fetch_sub(layout.size() as isize, Ordering::Relaxed);
            // SAFETY: `ptr` came from `alloc` above with `layout`, which is `System.alloc`'s, so
            // it is `System`'s to free with that layout.
            unsafe { System.dealloc(ptr, layout) }
        }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

struct Measured {
    parsed: Duration,
    held: isize,
    walked: Duration,
    held_walked: isize,
    nodes: Duration,
    found: Duration,
    elements: usize,
}

fn measure(texts: &[Arc<str>], outline: bool) -> Measured {
    let before = HELD.load(Ordering::Relaxed);
    let start = Instant::now();
    let parses: Vec<_> = texts
        .iter()
        .map(|it| match outline {
            true => SourceFile::parse_outline(it.clone(), Edition::CURRENT),
            false => SourceFile::parse_shared(it.clone(), Edition::CURRENT),
        })
        .collect();
    let parsed = start.elapsed();
    let held = HELD.load(Ordering::Relaxed) - before;
    let start = Instant::now();
    let mut elements = 0;
    for parse in &parses {
        for event in parse.tree().syntax().preorder_with_tokens() {
            if let WalkEvent::Enter(it) = event {
                std::hint::black_box((it.kind(), it.text_range()));
                elements += 1;
            }
        }
    }
    let walked = start.elapsed();
    let held_walked = HELD.load(Ordering::Relaxed) - before;
    let start = Instant::now();
    let mut ptrs = Vec::new();
    for parse in &parses {
        let mut of_file = Vec::new();
        for (i, node) in parse.tree().syntax().descendants().enumerate() {
            std::hint::black_box((node.kind(), node.text_range()));
            if i % 16 == 0 {
                of_file.push(SyntaxNodePtr::new(&node));
            }
        }
        ptrs.push(of_file);
    }
    let nodes = start.elapsed();
    let start = Instant::now();
    for (parse, of_file) in parses.iter().zip(&ptrs) {
        let root = parse.tree().syntax().clone();
        for ptr in of_file {
            std::hint::black_box(ptr.to_node(&root));
        }
    }
    let found = start.elapsed();
    // A parse's tree is dropped on a thread of its own, so the next count waits for it.
    drop(parses);
    let mut last = HELD.load(Ordering::Relaxed);
    loop {
        blaze_graph::spin::busy(Duration::from_millis(300));
        let now = HELD.load(Ordering::Relaxed);
        if now == last {
            break;
        }
        last = now;
    }
    Measured { parsed, held, walked, held_walked, nodes, found, elements }
}

fn main() {
    let mut paths = Vec::new();
    for dir in std::env::args().skip(1) {
        files(Path::new(&dir), &mut paths);
    }
    let texts: Vec<Arc<str>> =
        paths.iter().filter_map(|it| std::fs::read_to_string(it).ok()).map(Arc::from).collect();
    let bytes: usize = texts.iter().map(|it| it.len()).sum();
    let full = measure(&texts, false);
    let outline = measure(&texts, true);
    assert_eq!(full.elements, outline.elements, "the trees have the same elements");
    let ms = |it: Duration| it.as_secs_f64() * 1e3;
    let mb = |it: isize| it as f64 / (1 << 20) as f64;
    println!(
        "{} files, {:.1} MB of text, {} nodes and tokens",
        texts.len(),
        bytes as f64 / (1 << 20) as f64,
        full.elements
    );
    for (name, it) in [("full", &full), ("outline", &outline)] {
        println!(
            "{name:7}: parsed in {:8.1} ms, holds {:8.1} MB, walked in {:8.1} ms, \
             then holds {:8.1} MB, its nodes in {:8.1} ms, one pointer in 16 found in {:8.1} ms",
            ms(it.parsed),
            mb(it.held),
            ms(it.walked),
            mb(it.held_walked),
            ms(it.nodes),
            ms(it.found)
        );
    }
}
