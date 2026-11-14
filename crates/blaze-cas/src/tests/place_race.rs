//! Two stores (two daemons with different homes) place the same value at the same path of a
//! directory they share (the machine's std cache) at once: both places end Ok and the path ends
//! holding exactly the value's bytes, in every order the simulation delivers their effects in.
//! The planted bug (`Store::plant_clear_then_clone`) restores the old order (remove `to`, then
//! clone onto it), which loses to the other place's clone landing in between (`File exists`).

use blaze_sim::{Broke, Config as SimConfig, Tape};

use super::*;

/// How the value is placed, by its kind and length ([`Store::place`]).
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// An output past [`CLONE_FROM`]: cloned from the whole file each store keeps of it.
    Output,
    /// A hot value past [`EXTERNAL_FROM`]: cloned from its file of its own.
    External,
    /// A short value: its bytes written under a staged name and renamed onto the path.
    Written,
}

/// What is at the path before the two places.
#[derive(Clone, Copy, Debug)]
enum Before {
    Nothing,
    /// Another value's bytes, as an output of an earlier build.
    Other,
    /// The value's bytes already, as a place of an earlier run left them.
    Same,
}

#[derive(Clone, Copy, Debug)]
struct Race {
    seed: u64,
    latency_us: u64,
    shape: Shape,
    before: Before,
    /// The store that asks first.
    first: usize,
    /// Completions delivered before the second store asks.
    lead: u32,
}

fn value_of(shape: Shape, seed: u64) -> (Kind, Vec<u8>) {
    match shape {
        Shape::Output => (Kind::Output, noise(CHUNK_FROM as usize + 10, seed)),
        Shape::External => (Kind::Image, noise(EXTERNAL_FROM + 10, seed)),
        Shape::Written => (Kind::Code, noise(300, seed)),
    }
}

/// Runs one race; `Err` says how the oracle failed.
fn race(case: Race, plant: bool) -> Result<(), String> {
    let latency = Duration::from_micros(case.latency_us);
    let mut sim = Sim::new(case.seed, Faults { max_latency: latency, ..Faults::default() });
    let (kind, bytes) = value_of(case.shape, case.seed ^ 0x5eed);
    let mut stores = [("a/store", 1u64 << 40), ("b/store", 2 << 40)].map(|(dir, tags)| {
        let mut store = open_with(&mut sim, Config { dir: name(dir), layers: Vec::new(), tags, load_tags: tags + (1 << 32), loader: crate::Loader::Pumped });
        store.plant_clear_then_clone = plant;
        store.put(key(1), kind, &bytes).unwrap();
        flush(&mut sim, &mut store);
        store
    });
    let to = name("std/out/object-1280068726d9f00a.d");
    match case.before {
        Before::Nothing => {}
        Before::Other => sim.change(&to, b"an earlier build's output"),
        Before::Same => sim.change(&to, &bytes),
    }
    let mut effects = Vec::new();
    let mut tags = [None, None];
    tags[case.first] = stores[case.first].place(&key(1), to.clone(), &mut effects);
    let second = 1 - case.first;
    let (mut asked, mut seen, mut done) = (false, 0, Vec::new());
    // The bound of the loop, on the simulation's clock: a completion is at most `latency` (some
    // microseconds) away, so two places end in milliseconds, and a store that keeps asking for
    // effects without ending is an `Err` naming the race, not a test that never returns.
    let end = blaze_core::wait::Deadline::after(sim.now(), Duration::from_secs(60));
    loop {
        // The second store asks once the first's effects have had `lead` completions, or
        // once nothing is in flight, whichever comes first.
        if !asked && (seen >= case.lead || (sim.in_flight() == 0 && effects.is_empty())) {
            asked = true;
            tags[second] = stores[second].place(&key(1), to.clone(), &mut effects);
        }
        sim.submit(&mut effects);
        if sim.in_flight() == 0 {
            match asked {
                true => break,
                false => continue,
            }
        }
        sim.poll(&mut done, end);
        // Effects are in flight (checked above), so a poll that delivers none either reached
        // `end` or holds effects that will never complete: both end the race.
        if done.is_empty() {
            return Err(format!(
                "the places did not end by {end:?} on the simulation's clock (now {:?}): {} \
                 effects in flight and none completed",
                sim.now(),
                sim.in_flight()
            ));
        }
        for completion in done.drain(..) {
            seen += 1;
            if let Some(store) = stores.iter_mut().find(|store| store.owns(completion.tag)) {
                store.complete(completion, &mut effects);
            }
        }
    }
    for (n, store) in stores.iter_mut().enumerate() {
        let tag = tags[n].ok_or_else(|| format!("store {n} found no value to place"))?;
        let placed = store.placed();
        match placed.iter().find(|placed| placed.tag == tag) {
            Some(Placed { result: Ok(()), .. }) => {}
            Some(Placed { result: Err(why), .. }) => {
                return Err(format!("store {n}'s place failed: {why}"));
            }
            None => return Err(format!("store {n}'s place never ended: {placed:?}")),
        }
        let temp = store.staging_name(&to, tag);
        if !plant && sim.exists(&temp) {
            return Err(format!("store {n}'s staged clone {temp} was left behind"));
        }
    }
    match sim.contents(&to) == bytes {
        true => Ok(()),
        false => Err(format!("{to} does not hold the value's bytes")),
    }
}

fn race_draw(tape: &Tape) -> Race {
    let shape = [Shape::Output, Shape::External, Shape::Written][usize::from(tape.int_in(0..=2u8, "shape"))];
    let before = [Before::Nothing, Before::Other, Before::Same][usize::from(tape.int_in(0..=2u8, "before"))];
    Race {
        seed: tape.next_u64("sim seed"),
        latency_us: tape.int_in(1..=2000u64, "latency us"),
        shape,
        before,
        first: usize::from(tape.int_in(0..=1u8, "first")),
        lead: tape.int_in(0..=12u32, "lead"),
    }
}

/// The invariant: any schedule of two stores placing the same value at one shared path ends with
/// both Ok and the path holding the value. `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn two_stores_placing_one_value_at_one_path_both_succeed_in_any_order() {
    blaze_sim::check(SimConfig::cases(64), |tape| {
        let case = race_draw(tape);
        if let Err(why) = race(case, false) {
            panic!("{case:?}: {why}");
        }
    });
}

/// The planted-bug control: with the old remove-then-clone-onto-`to` order switched back on,
/// the same oracle fails under some schedule of the same draws, with the error seen live. Its
/// cases are the 512 races it searched by hand before (64 seeds, two shapes, four leads).
#[test]
fn the_oracle_catches_a_remove_then_clone_onto_the_path() {
    blaze_sim::control(SimConfig::cases(512), false, true, "already there", |tape, plant| {
        let seed = tape.next_u64("sim seed");
        let it = Race {
            seed,
            latency_us: 200,
            shape: *tape.one_of(&[Shape::Output, Shape::External], "shape"),
            before: Before::Nothing,
            first: (seed % 2) as usize,
            lead: *tape.one_of(&[0, 1, 2, 4], "lead"),
        };
        race(it, plant).map_err(|why| Broke::Oracle(format!("{it:?}: {why}")))
    });
}
