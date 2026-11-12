//! The store's whole file of an output takes its content's name whole or not at all: it is
//! written under a writing name, sealed, written back, and only then renamed. So what a crash
//! leaves under a content's name is the content, and a file there with other bytes was changed
//! outside the daemon (which the store refuses, naming `blaze store verify`); what a crash
//! leaves under a writing name the next open sweeps.

use super::*;

/// An output placed once, the power going out after `polls` rounds of completions; then the
/// store opened again. `Err` says what the oracle found. `plant`: the whole file is written
/// in place, under the content's own name.
fn crashed(seed: u64, polls: u32, plant: bool) -> Result<(), String> {
    let mut sim = Sim::new(seed, quiet());
    let mut store = open(&mut sim, "store");
    if plant {
        store.plant = PlacePlant::WritesInPlace;
    }
    let out = noise(CHUNK_FROM + 10 + (seed as usize % 7) * 4096, seed);
    store.put(key(1), Kind::Output, &out).unwrap();
    flush(&mut sim, &mut store);
    let content = content_of(&out);
    let file = name(&format!("store/placed/{content:x}"));
    let writing = name(&format!("store/placed/{content:x}{WRITING_SUFFIX}"));
    let mut effects = Vec::new();
    store.place(&key(1), name("one/build/a"), &mut effects).ok_or("no value to place")?;
    for _ in 0..polls {
        sim.submit(&mut effects);
        let mut done = Vec::new();
        sim.poll(&mut done, blaze_core::wait::Deadline::NEVER);
        for completion in done {
            if store.owns(completion.tag) {
                store.complete(completion, &mut effects);
            }
        }
    }
    sim.crash();
    drop(store);
    // The oracle: whatever the crash cut, the content's own name holds the content or
    // nothing.
    let whole = |sim: &Sim| !sim.exists(&file) || content_of(&sim.contents(&file)) == content;
    if !whole(&sim) {
        return Err(format!(
            "a crash after {polls} rounds left {file} holding {} bytes that are not its name's",
            sim.contents(&file).len()
        ));
    }
    let mut store = open(&mut sim, "store");
    if sim.exists(&writing) {
        return Err(format!("{writing} was not swept as the store opened"));
    }
    if !whole(&sim) {
        return Err(format!("after the open {file} holds bytes that are not its name's"));
    }
    // The build after the restart places it: from the whole file the crash left, or written.
    let mut effects = Vec::new();
    store.place(&key(1), name("two/build/a"), &mut effects).ok_or("no value after the crash")?;
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    if placed.len() != 1 || placed[0].result != Ok(()) {
        return Err(format!("the place after the restart: {placed:?}"));
    }
    if sim.contents(&name("two/build/a")) != out {
        return Err("the place after the restart does not hold the value's bytes".into());
    }
    if !whole(&sim) || !sim.exists(&file) {
        return Err(format!("{file} is not there whole after the place"));
    }
    Ok(())
}

/// The invariant: after a crash at any point of a place, and after the open that follows, no
/// whole file of the store under a content's name holds other bytes than that content's; a
/// writing name left behind is swept by the open; and the next place succeeds.
#[test]
fn a_crash_at_any_point_leaves_no_placed_file_with_bytes_that_are_not_its_names() {
    for seed in 0..24 {
        for polls in 0..40 {
            assert_eq!(crashed(seed, polls, false), Ok(()), "seed {seed}, {polls} rounds");
        }
    }
}

/// The planted-bug control: written in place under the content's own name, a crash before
/// its write-back leaves part of it there, and the oracle says so.
#[test]
fn the_oracle_catches_a_whole_file_written_in_place() {
    let mut caught = None;
    for seed in 0..24 {
        for polls in 0..40 {
            if caught.is_none()
                && let Err(why) = crashed(seed, polls, true)
            {
                caught = Some((seed, polls, why));
            }
        }
    }
    let (seed, polls, why) = caught.expect("the oracle missed the whole file written in place");
    assert!(why.contains("bytes that are not its name's"), "seed {seed}, {polls} rounds: {why}");
}
