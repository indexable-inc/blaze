//! [`Parent::toggle`] against a viewer that turns hooks on and off for functions and sets of
//! them while threads call them, lookups and compiles ending in any order.
//!
//! The oracle: every code a slot is given computes what that slot's function computes (a hook
//! adds what it observes, never changes the result); a slot is never given the code it runs
//! again; once everything asked is done, each called slot runs the version with exactly the
//! hooks the toggles asked for, and one with every hook off runs its plain version, the very
//! record it ran before any toggle.

use std::alloc::Global;
use std::collections::BTreeMap;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::key;
use super::oracle;
use super::parent::functions;
use crate::parent::{Compiled, Config as ParentConfig, Effect, FastRecord, Found, Parent};
use crate::viewer;
use crate::{CodeRef, Hooks, Nanos, Plant, SlotId, ThreadId, Version};

const FUNCTIONS: u32 = 3;
const HOOKS: [Hooks; 3] = [Hooks::TRACE, Hooks::COUNTS, Hooks::BREAK];

/// The record the compiler makes for a version: its slot and hooks are in its key.
fn code_of(version: Version) -> CodeRef {
    CodeRef(key(10_000 + u64::from(version.slot.0) * 64 + u64::from(version.hooks.0)))
}

/// What running `code` computes: its function's result, whatever hooks it has.
fn computes(code: CodeRef) -> u64 {
    let n = u64::from_le_bytes(code.0[..8].try_into().unwrap_or([0; 8]));
    (n - 10_000) / 64 * 7 + 1
}

fn refused(error: crate::parent::Error) -> Broke {
    Broke::Oracle(format!("the parent refused a call the schedule may make: {error}"))
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let functions: Vec<_> = functions().into_iter().take(FUNCTIONS as usize).collect();
    let config = ParentConfig { workers: 2, compile_bound: Nanos(u64::MAX / 4), most_waiters: 64 };
    let mut parent = Parent::new(&functions, config, Global).map_err(refused)?;
    parent.plant = plant;
    let mut out = Vec::new_in(Global);
    parent.start(&[], &[], Nanos(0), &mut out).map_err(refused)?;
    let mut wanted = [Hooks::NONE; FUNCTIONS as usize];
    let mut words: [Option<CodeRef>; FUNCTIONS as usize] = [None; FUNCTIONS as usize];
    let mut lookups: Vec<Version> = Vec::new();
    let mut compiles: Vec<Version> = Vec::new();
    let mut waiting: BTreeMap<ThreadId, SlotId> = BTreeMap::new();
    let steps = tape.range(1..100, "steps");
    let mut step = 0;
    loop {
        for effect in out.drain(..) {
            match effect {
                Effect::Lookup { version, .. } => lookups.push(version),
                Effect::Compile { version, .. } => compiles.push(version),
                Effect::Publish { slot, code, .. } => {
                    let function = slot.0 as u64 * 7 + 1;
                    oracle(computes(code) == function, || {
                        format!("a toggle changed what slot {} computes: {} for {function}", slot.0, computes(code))
                    })?;
                    oracle(words[slot.index()] != Some(code), || {
                        format!("slot {} was given the code it runs again", slot.0)
                    })?;
                    words[slot.index()] = Some(code);
                }
                Effect::Answer { thread, .. } => {
                    waiting.remove(&thread);
                }
                Effect::Make { .. } | Effect::Untoggled { .. } | Effect::NoStart(_) | Effect::Stop(_) => {
                    return Err(Broke::Harness(format!("{effect:?} with no failure and nothing lacking")));
                }
                Effect::Begin | Effect::Say(_) => {}
            }
        }
        step += 1;
        if step >= steps && lookups.is_empty() && compiles.is_empty() {
            break;
        }
        let action = if step < steps { tape.below(5, "action") } else { 2 + step % 2 };
        match action {
            0 => {
                let thread = ThreadId(tape.below(4, "thread"));
                if waiting.contains_key(&thread) {
                    continue;
                }
                let slot = SlotId(tape.below(u64::from(FUNCTIONS), "called") as u32);
                // A call reaches the parent only through a stub.
                if words[slot.index()].is_some() {
                    continue;
                }
                waiting.insert(thread, slot);
                parent.need(thread, slot, Nanos(step), &mut out).map_err(refused)?;
            }
            1 => {
                let hooks = HOOKS[tape.index(HOOKS.len(), "hook")];
                let on = tape.coin("on");
                let set: Vec<SlotId> = (0..FUNCTIONS).filter(|_| tape.coin("in the set")).map(SlotId).collect();
                for slot in &set {
                    wanted[slot.index()] = wanted[slot.index()].with(hooks, on);
                }
                parent.toggle(&set, hooks, on, &mut out).map_err(refused)?;
            }
            2 if !lookups.is_empty() => {
                let version = lookups.remove(tape.index(lookups.len(), "lookup"));
                let found = tape.coin("in the store").then(|| Found { fast: code_of(version), callees: &[], optimized: None });
                parent.looked(version, found, &mut out).map_err(refused)?;
            }
            3 if !compiles.is_empty() => {
                let version = compiles.remove(tape.index(compiles.len(), "compile"));
                let record = FastRecord { code: code_of(version), callees: Vec::new_in(Global), reads: Vec::new_in(Global) };
                parent.compiled(version, Compiled::Record(&record), Nanos(step), &mut out).map_err(refused)?;
            }
            _ => continue,
        }
    }
    oracle(waiting.is_empty(), || format!("{} calls were never answered", waiting.len()))?;
    for at in 0..FUNCTIONS {
        let slot = SlotId(at);
        let Some(code) = words[slot.index()] else { continue };
        let want = wanted[slot.index()];
        let plain = code_of(Version { slot, hooks: Hooks::NONE });
        if want.is_none() {
            oracle(code == plain, || format!("off did not return slot {} to its plain version", slot.0))?;
        } else {
            oracle(code == code_of(Version { slot, hooks: want }), || {
                format!("a toggle left slot {} running other hooks than {:#x}", slot.0, want.0)
            })?;
        }
    }
    Ok(())
}

#[test]
fn a_toggle_changes_what_runs_never_what_it_computes() {
    check(Config::cases(500), |tape| run(tape, Plant::None));
}

/// The viewer's toggle reads back as written, and a request in any other form of the same
/// value (a longer head) is refused: one encoding a value.
#[test]
fn a_toggle_request_reads_back_and_only_its_shortest_form_does() {
    check(Config::cases(300), |tape| {
        let hooks = Hooks(tape.next_u32("hooks") as u16);
        let on = tape.coin("on");
        let slots: Vec<SlotId> = (0..tape.below(6, "slots")).map(|_| SlotId(tape.next_u32("slot"))).collect();
        let mut bytes = Vec::new_in(Global);
        viewer::toggle(&slots, hooks, on, &mut bytes);
        let mut back = Vec::new_in(Global);
        let read = viewer::read(&bytes, 8, &mut back);
        oracle(read == Ok(viewer::Request::Toggle { hooks, on }) && back.as_slice() == slots.as_slice(), || {
            format!("{slots:?} {hooks:?} {on} read back as {read:?} {back:?}")
        })?;
        // The version, 1, as a one-byte head (0x18 0x01): the same value, not the shortest form.
        let mut longer = Vec::new_in(Global);
        longer.push(bytes[0]);
        longer.extend_from_slice(&[0x18, 0x01]);
        longer.extend_from_slice(&bytes[2..]);
        let mut ignored = Vec::new_in(Global);
        let refused = viewer::read(&longer, 8, &mut ignored);
        oracle(refused == Err(viewer::Bad::NotDeterministic { at: 1 }), || {
            format!("a longer head was read as {refused:?}")
        })?;
        let mut few = Vec::new_in(Global);
        let capped = viewer::read(&bytes, 0, &mut few);
        oracle(slots.is_empty() || matches!(capped, Err(viewer::Bad::TooMany { .. })), || {
            format!("the slot cap was not kept: {capped:?}")
        })
    });
}

#[test]
fn the_oracle_catches_a_slot_left_hooked_after_off() {
    control(Config::cases(4000), Plant::None, Plant::OffKeepsHooks, "off did not return", run);
}

#[test]
fn the_oracle_catches_versions_keyed_without_their_function() {
    control(Config::cases(4000), Plant::None, Plant::VersionsByHooks, "a toggle changed what", run);
}
