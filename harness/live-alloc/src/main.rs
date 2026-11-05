// harness/live, on the nightly's `allocator_api`: what the loop computes lives in a bump arena
// of its own, through `Vec<T, A>` and `Box<T, A>`, as the game's arenas do.
#![feature(allocator_api)]

use std::{
    alloc::{AllocError, Allocator, Global, Layout},
    cell::Cell,
    io::Write,
    ptr::NonNull,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[inline(never)]
fn answer() -> u64 {
    1
}

/// A bump arena: a block from the global allocator, handed out front to back, freed whole.
struct Bump {
    block: NonNull<u8>,
    size: usize,
    used: Cell<usize>,
}

impl Bump {
    const BLOCK: Layout = match Layout::from_size_align(1 << 16, 64) {
        Ok(it) => it,
        Err(_) => panic!(),
    };

    fn new() -> Bump {
        let block = Global.allocate(Self::BLOCK).expect("the arena's block");
        Bump { block: block.cast(), size: Self::BLOCK.size(), used: Cell::new(0) }
    }

    fn reset(&mut self) {
        self.used.set(0);
    }
}

unsafe impl Allocator for &Bump {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let start = self.used.get().next_multiple_of(layout.align());
        let end = start.checked_add(layout.size()).filter(|&it| it <= self.size).ok_or(AllocError)?;
        self.used.set(end);
        let at = unsafe { self.block.add(start) };
        Ok(NonNull::slice_from_raw_parts(at, layout.size()))
    }

    unsafe fn deallocate(&self, _: NonNull<u8>, _: Layout) {}
}

impl Drop for Bump {
    fn drop(&mut self) {
        unsafe { Global.deallocate(self.block, Self::BLOCK) };
    }
}

/// The turn's sum, built in the arena: the answer boxed in it and a vector of weights.
#[inline(never)]
fn turn(arena: &Bump, turns: u64) -> u64 {
    let boxed: Box<u64, &Bump> = Box::new_in(answer(), arena);
    let mut weights: Vec<u64, &Bump> = Vec::with_capacity_in(4, arena);
    weights.extend([1, 2, 3, turns % 7]);
    std::hint::black_box(weights.iter().sum::<u64>());
    *boxed
}

fn main() {
    let live = match blaze_live::Live::start() {
        Ok(live) => live,
        Err(why) => {
            println!("no live edits: {why}");
            std::process::exit(2);
        }
    };
    live.apply_as_they_come(|outcome| {
        let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        println!("outcome: {outcome} at_us={at}");
        std::io::stdout().flush().unwrap();
    });
    println!("ready");
    let mut arena = Bump::new();
    let mut seen = 0;
    let mut turns = 0;
    loop {
        turns += 1;
        let now = turn(&arena, turns);
        arena.reset();
        if now != seen {
            let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
            println!("answer {now} at_us={at}");
            seen = now;
        }
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
