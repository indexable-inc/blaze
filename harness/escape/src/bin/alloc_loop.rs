//! `step` under std's allocator: blaze moves its three allocations to the stack.

include!("../step.rs");

fn main() {
    let (n, acc, elapsed) = run();
    println!("steps {n} result {acc} elapsed_ns {}", elapsed.as_nanos());
}
