# blaze-live

Edit Rust and the running program has the new code about a millisecond later. Its state stays.

The runtime half of [blaze](https://github.com/indexable-inc/blaze): link it, poll once per
frame, and run your program under the blaze-engine daemon.

```toml
[dependencies]
blaze-live = "0.1"
```

```rust
let mut live = blaze_live::Live::start().ok();
loop {
    if let Some(outcome) = live.as_mut().and_then(|it| it.poll()) {
        eprintln!("blaze: {outcome}");
    }
    // ...your frame
}
```

Every function a patch can replace is called through a slot of a jump table the linker laid
out; a patch is taken in one atomic store of the table, with no thread stopped and no code a
thread can be in written over.

Linux on x86-64 and AArch64, macOS on Apple silicon and Windows x86-64; elsewhere
`Live::start` says so and you still get blaze's fast builds. MIT or Apache-2.0.
