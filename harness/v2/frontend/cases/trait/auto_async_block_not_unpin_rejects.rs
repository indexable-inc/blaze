//@ expect: reject E0277
//@ edition: 2021
// An async block is an immovable coroutine, never Unpin, even with no await in it.
fn is_unpin<T: Unpin>(_: T) {}

fn main() {
    is_unpin(async { 1u8 });
}
