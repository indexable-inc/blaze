//! [`Parent::toggle`] against a viewer that turns hooks on and off for functions and sets of
//! them while threads call them, lookups and compiles ending in any order.
//!
//! The oracle: a toggle compiles and looks up nothing (a hook is a stub, never code); every code
//! a slot is given is its one record, keyed by function and tier alone, and never the code it
//! runs again; at every step each slot's hooks, as the program was last told them, are exactly
//! what the toggles asked for, and a slot with every hook off was told so.

use std::collections::BTreeMap;

use blaze_own::{Alloc, Vec as Own};
use blaze_sim::{Broke, Config, Tape, check, control};

use super::key;
use super::oracle;
use super::parent::functions;
use crate::parent::{Compiled, Config as ParentConfig, Effect, FastRecord, Found, Parent};
use crate::viewer;
use crate::{CodeRef, Hooks, Nanos, Plant, SlotId, ThreadId};

const FUNCTIONS: u32 = 3;
const HOOKS: [Hooks; 3] = [Hooks::TRACE, Hooks::COUNTS, Hooks::BREAK];

/// A slot's one code record.
fn code_of(slot: SlotId) -> CodeRef {
    CodeRef(key(10_000 + u64::from(slot.0)))
}

fn refused(error: crate::parent::Error) -> Broke {
    Broke::Oracle(format!("the parent refused a call the schedule may make: {error}"))
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let functions: Vec<_> = functions().into_iter().take(FUNCTIONS as usize).collect();
    let config = ParentConfig { workers: 2, compile_bound: Nanos(u64::MAX / 4), most_waiters: 64 };
    let mut parent = Parent::new(&functions, config, Alloc::global()).map_err(refused)?;
    parent.plant = plant;
    let mut out = Own::new_in(Alloc::global());
    parent.start(&[], &[], Nanos(0), &mut out).map_err(refused)?;
    let mut wanted = [Hooks::NONE; FUNCTIONS as usize];
    let mut told = [Hooks::NONE; FUNCTIONS as usize];
    let mut words: [Option<CodeRef>; FUNCTIONS as usize] = [None; FUNCTIONS as usize];
    let mut lookups: Vec<SlotId> = Vec::new();
    let mut compiles: Vec<SlotId> = Vec::new();
    let mut waiting: BTreeMap<ThreadId, SlotId> = BTreeMap::new();
    let steps = tape.range(1..100, "steps");
    let mut step = 0;
    let mut toggled = false;
    loop {
        for effect in out.drain(..) {
            match effect {
                Effect::Lookup { slot, .. } | Effect::Compile { slot, .. } if toggled => {
                    return Err(Broke::Oracle(format!("a toggle looked up or compiled slot {}", slot.0)));
                }
                Effect::Lookup { slot, .. } => lookups.push(slot),
                Effect::Compile { slot, .. } => compiles.push(slot),
                Effect::Publish { slot, code, .. } => {
                    oracle(code == code_of(slot), || format!("slot {} was given code not its own", slot.0))?;
                    oracle(words[slot.index()] != Some(code), || {
                        format!("slot {} was given the code it runs again", slot.0)
                    })?;
                    words[slot.index()] = Some(code);
                }
                Effect::Hooks { slot, hooks } => told[slot.index()] = hooks,
                Effect::Answer { thread, .. } => {
                    waiting.remove(&thread);
                }
                Effect::Make { .. } | Effect::NoStart(_) | Effect::Stop(_) => {
                    return Err(Broke::Harness(format!("{effect:?} with no failure and nothing lacking")));
                }
                Effect::Begin | Effect::Say(_) | Effect::WriteImage => {}
                Effect::Declare { .. } | Effect::Unresolved { .. } => {
                    return Err(Broke::Harness(format!("{effect:?} from a parent asked by slot alone")));
                }
            }
        }
        for at in 0..FUNCTIONS as usize {
            let (want, got) = (wanted[at], told[at]);
            if want.is_none() {
                oracle(got.is_none(), || format!("off did not return slot {at} to no hooks: {:#x}", got.0))?;
            } else {
                oracle(got == want, || format!("slot {at} runs hooks {:#x}, not the {:#x} asked", got.0, want.0))?;
            }
        }
        toggled = false;
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
                toggled = true;
            }
            2 if !lookups.is_empty() => {
                let slot = lookups.remove(tape.index(lookups.len(), "lookup"));
                let found = tape.coin("in the store").then(|| Found { fast: code_of(slot), callees: &[], optimized: None });
                parent.looked(slot, found, &mut out).map_err(refused)?;
            }
            3 if !compiles.is_empty() => {
                let slot = compiles.remove(tape.index(compiles.len(), "compile"));
                let record = FastRecord { code: code_of(slot), callees: Own::new_in(Alloc::global()), reads: Own::new_in(Alloc::global()) };
                parent.compiled(slot, Compiled::Record(&record), Nanos(step), &mut out).map_err(refused)?;
            }
            _ => continue,
        }
    }
    oracle(waiting.is_empty(), || format!("{} calls were never answered", waiting.len()))
}

/// The invariant: toggling hooks compiles and looks up nothing and leaves every slot's active hooks equal to the exact set currently turned on.
#[test]
fn a_toggle_changes_only_the_hooks_and_compiles_nothing() {
    check(Config::cases(500), |tape| run(tape, Plant::None));
}

/// The viewer's toggle reads back as written, and a request in any other form of the same
/// value (a longer head) is refused: one encoding a value.
#[test]
fn a_toggle_request_reads_back_and_only_its_shortest_form_does() {
    check(Config::cases(300), |tape| {
        let hooks = Hooks(tape.next_u32("hooks") as u16 & Hooks::ALL.0);
        let on = tape.coin("on");
        let slots: Vec<SlotId> = (0..tape.below(6, "slots")).map(|_| SlotId(tape.next_u32("slot"))).collect();
        let mut bytes = Own::new_in(Alloc::global());
        viewer::toggle(&slots, hooks, on, &mut bytes);
        let mut back = Own::new_in(Alloc::global());
        let read = viewer::read(&bytes, 8, &mut back);
        oracle(read == Ok(viewer::Request::Toggle { hooks, on }) && back.as_slice() == slots.as_slice(), || {
            format!("{slots:?} {hooks:?} {on} read back as {read:?} {back:?}")
        })?;
        // The version, 1, as a one-byte head (0x18 0x01): the same value, not the shortest form.
        let mut longer = Own::new_in(Alloc::global());
        longer.push(bytes[0]);
        longer.extend_from_slice(&[0x18, 0x01]);
        longer.extend_from_slice(&bytes[2..]);
        let mut ignored = Own::new_in(Alloc::global());
        let refused = viewer::read(&longer, 8, &mut ignored);
        oracle(refused == Err(viewer::Bad::NotDeterministic { at: 1 }), || {
            format!("a longer head was read as {refused:?}")
        })?;
        let mut few = Own::new_in(Alloc::global());
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
