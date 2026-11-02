## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng` (report `/tmp/blaze-lanes/v2-dedup-codec-rng.md`). `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`) is the one seeded test RNG: `new`, `next_u64`, `below` (`% n`, 0 for 0), `index`, `one_in`, `shuffle`, `fill`, and more. Each local copy below gives the same stream; please delete it and use the shared one.

`crates/blaze-core/src/sim.rs` keeps its `Rng` (public through `sim::Program`, and `blaze-core` stays below `blake3`: docs/v2/plan/lanes/dedup.md keeps `blaze-radix` separate for that reason). Only its comment changes, so a reader knows it is a kept copy:

Before: `/// SplitMix64: small, fast, and the same everywhere.`
After: `/// SplitMix64, a copy of `blaze_ids::rng::SplitMix64` (the same stream from a seed): `blaze-core` stays below `blake3`, so it can't depend on `blaze-ids`.`

> forwarded to mem-core-sim (from requests, pass 16; told by tmux).

## mem-core-macos: threads.rs's macOS `Watching` onto `fsevents::Watch` (needed to build on macOS)

From `mem-core-macos` (report `/tmp/blaze-lanes/v2-mem-core-macos.md`). `crates/blaze-core/src/fsevents.rs` no longer has `Stream` (`start`/`journal` taking a `Box<dyn Fn(Completion)>`, `stop(self)`). It has `Watch<S: Tell>`: made once (`Watch::new(sink)`: cookie directory, dispatch queue, buffers), started and stopped in place any number of times, and it must not move while a stream runs, which is why it is boxed below (`Watching` lives in an `FxHashMap` that moves its values). `Tell` is implemented for `std::sync::mpsc::Sender<Completion>` and `crossbeam_channel::Sender<Completion>`, so the sink is `finished` itself. `journal` hands its first batch to a closure just before the stream starts, so it is told before anything the stream tells, as before. Since checkpoint 80a00d68fc HEAD has the new fsevents.rs and the old threads.rs, so blaze-core does not build on macOS until this lands.

Replace the `#[cfg(target_os = "macos")] struct Watching` and its `impl` (threads.rs, "A watch of FSEvents', which calls back on a queue of its own.") with:

```rust
/// A watch of FSEvents', which calls back on a queue of its own.
#[cfg(target_os = "macos")]
struct Watching {
    watch: std::sync::Mutex<Box<crate::fsevents::Watch<Sender<Completion>>>>,
    tag: Tag,
    finished: Sender<Completion>,
}

#[cfg(target_os = "macos")]
impl Watching {
    fn start(
        root: &Path,
        dir: &crate::FileName,
        tag: Tag,
        finished: Sender<Completion>,
    ) -> io::Result<Watching> {
        // alloc: setup, once per watch; boxed so it stays where it is while its stream runs.
        let mut watch = Box::new(crate::fsevents::Watch::new(finished.clone())?);
        // SAFETY: boxed, the watch does not move until `stop` or its drop stops it.
        unsafe { watch.start(root, dir.clone(), tag) }?;
        Ok(Watching { watch: std::sync::Mutex::new(watch), tag, finished })
    }

    /// An [`Effect::Journal`]: FSEvents' stream from the event `since` names.
    fn journal(
        root: &Path,
        dir: &crate::FileName,
        since: crate::Cursor,
        tag: Tag,
        finished: Sender<Completion>,
    ) -> io::Result<Watching> {
        // alloc: setup, once per watch; boxed so it stays where it is while its stream runs.
        let mut watch = Box::new(crate::fsevents::Watch::new(finished.clone())?);
        // alloc: the batch's payload.
        let first = |first| _ = finished.send(Completion { tag, done: Done::Journaled(Box::new(first)) });
        // SAFETY: as in `start`.
        unsafe { watch.journal(root, dir.clone(), since, tag, first) }?;
        Ok(Watching { watch: std::sync::Mutex::new(watch), tag, finished })
    }

    fn stop(&self) {
        let mut watch = self.watch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if watch.is_running() {
            let finished = &self.finished;
            watch.stop(|flush| {
                // alloc: an error path.
                let why = IoError("the watch ended before the flush".to_owned());
                _ = finished.send(Completion { tag: flush, done: Done::Failed(why) });
            });
            _ = self.finished.send(Completion { tag: self.tag, done: Done::Unwatched });
        }
    }

    /// Asks the stream for a flush `tag`, which it answers through `finished`.
    fn flush(&self, tag: Tag, finished: &Sender<Completion>) {
        let watch = self.watch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = watch.flush(tag) {
            // alloc: an error path.
            let why = IoError(format!("flushing the watch: {error}"));
            _ = finished.send(Completion { tag, done: Done::Failed(why) });
        }
    }

    fn join(self) {}
}
```

Behavior change: a flush still waiting when the watch stops is now told `Failed` (before, `Stream::stop` dropped it untold, leaving it in flight).

> forwarded to mem-core-sim (from requests, pass 16; told by tmux).

## mem-core-sim: both forwarded requests are applied (nothing for the coordinator)

- `dedup-codec-rng`'s comment on `sim::Rng` is in `crates/blaze-core/src/sim.rs`.
- `mem-core-macos`'s `Watching`: threads.rs holds `fsevents::Watch<Sink>` in its watch slab
  entry, stopped in place by `retire` and kept in `spare` (no box needed: a slab entry never
  moves); a flush still waiting when the watch stops is told `Failed`, as asked.

> applied: nothing to apply (a report) (requests, pass 19).
