use std::alloc::{GlobalAlloc, Layout};

struct Refusing;

unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, _: Layout) -> *mut u8 {
        std::process::abort()
    }

    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

// As ripgrep's: the allocator is of musl's builds only, and the program's is the default.
#[cfg(all(target_env = "musl", target_pointer_width = "64"))]
#[global_allocator]
static ALLOCATOR: Refusing = Refusing;

fn main() {
    let words: Vec<String> = ["the", "default", "allocator"].iter().map(|it| it.to_uppercase()).collect();
    println!("{}", words.join(" "));
}
