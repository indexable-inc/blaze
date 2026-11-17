//! blaze-id's interface: what [`NoItem`] says, a key's shape, and [`Carried`] against a model
//! under any operations, with the control its oracle must catch. `Portable`'s refusal of an
//! interned id is the `compile_fail` doc test on `Portable`, beside the doc test that compiles.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_id::{Carried, ContentId, CrateItemKey, ItemKey, NoItem};
use blaze_sim::{Broke, Config, Tape};

/// A key whose 32 bytes are each `byte`.
fn key(byte: u8) -> ItemKey {
    ItemKey(ContentId([byte; 32]))
}

/// The error names the key and the crate, each in full.
#[test]
fn no_item_names_its_key_and_its_crate() {
    let mut item = [0xab; 32];
    item[31] = 0x01;
    let error = NoItem { krate: CrateItemKey(ContentId([0x12; 32])), key: ItemKey(ContentId(item)) };
    assert_eq!(
        error.to_string(),
        "the item key ababababababababababababababababababababababababababababababab01 names no \
         item of the crate 1212121212121212121212121212121212121212121212121212121212121212"
    );
}

/// The first 8 bytes are the crate's, and the top bit of byte 8 alone says inner.
#[test]
fn a_key_names_its_crate_and_whether_it_is_inner() {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let mut krate = [0u8; 32];
    krate[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    krate[8] = 0xff;
    let outer = ItemKey(ContentId(bytes));
    bytes[8] = ItemKey::INNER | 0x01;
    let inner = ItemKey(ContentId(bytes));
    assert_eq!(outer.crate_prefix(), [1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(!outer.is_inner());
    assert!(inner.is_inner());
    let of = CrateItemKey(ContentId(krate));
    assert!(outer.is_of(of) && inner.is_of(of));
    krate[7] = 0;
    assert!(!outer.is_of(CrateItemKey(ContentId(krate))));
}

/// One operation on a table and its model.
#[derive(Clone, Copy, Debug)]
enum Op {
    Insert(u8, u32),
    Remove(u8),
    Get(u8),
    /// Keeps the entries whose value is even.
    RetainEven,
}

fn op(tape: &Tape) -> Op {
    // Eight keys, so inserts land on keys already there and removes find something.
    let at = u8::try_from(tape.below(8, "key")).expect("a draw below 8 fits a u8");
    match tape.below(4, "op") {
        0 => Op::Insert(at, u32::try_from(tape.below(1000, "value")).expect("below 1000 fits a u32")),
        1 => Op::Remove(at),
        2 => Op::Get(at),
        _ => Op::RetainEven,
    }
}

/// A bug planted in how the table is driven, which the oracle must catch.
#[derive(Clone, Copy, Debug)]
enum Plant {
    None,
    /// A remove is not done on the table: an entry stays that the model dropped.
    RemoveForgotten,
}

/// The reference model: a list of pairs, at most one per key.
struct Model(Vec<(ItemKey, u32), Global>);

impl Model {
    fn place(&self, key: ItemKey) -> Option<usize> {
        self.0.iter().position(|&(it, _)| it == key)
    }
}

/// Runs `ops` on a table and the model, checking after each that both hold the same entries
/// and agree on what each operation gave back.
fn agrees(ops: &[Op], plant: Plant) -> Result<(), Broke> {
    let mut table: Carried<ItemKey, u32, Global> = Carried::new_in(Global);
    let mut model = Model(Vec::new_in(Global));
    for &op in ops {
        match op {
            Op::Insert(at, value) => {
                let gave = table.insert(key(at), value);
                let had = match model.place(key(at)) {
                    Some(place) => Some(std::mem::replace(&mut model.0[place].1, value)),
                    None => {
                        model.0.push((key(at), value));
                        None
                    }
                };
                if gave != had {
                    return Err(Broke::Oracle(format!("insert of {at} gave {gave:?}, the model {had:?}")));
                }
            }
            Op::Remove(at) => {
                let gave = match plant {
                    Plant::RemoveForgotten => table.get(&key(at)).copied(),
                    Plant::None => table.remove(&key(at)),
                };
                let had = model.place(key(at)).map(|place| model.0.swap_remove(place).1);
                if gave != had {
                    return Err(Broke::Oracle(format!("remove of {at} gave {gave:?}, the model {had:?}")));
                }
            }
            Op::Get(at) => {
                let gave = table.get(&key(at)).copied();
                let had = model.place(key(at)).map(|place| model.0[place].1);
                if gave != had || table.contains_key(&key(at)) != had.is_some() {
                    return Err(Broke::Oracle(format!("get of {at} gave {gave:?}, the model {had:?}")));
                }
            }
            Op::RetainEven => {
                table.retain(|_, value| *value % 2 == 0);
                model.0.retain(|&(_, value)| value % 2 == 0);
            }
        }
        let mut held = Vec::with_capacity_in(table.len(), Global);
        held.extend(table.iter().map(|(&it, &value)| (it, value)));
        held.sort_unstable();
        let mut want = Vec::with_capacity_in(model.0.len(), Global);
        want.extend(model.0.iter().copied());
        want.sort_unstable();
        if held != want || table.len() != want.len() || table.is_empty() != want.is_empty() {
            return Err(Broke::Oracle(format!("the table holds {held:?}, the model {want:?}")));
        }
    }
    Ok(())
}

/// The invariant: after any inserts, removes, lookups and retains, a carried table holds the
/// entries a list of pairs under the same operations holds, and each operation gives back
/// what the list's does.
#[test]
fn a_carried_table_is_a_list_of_pairs_under_any_operations() {
    blaze_sim::check(Config::cases(300), |tape| agrees(&tape.vec(0..=40, "ops", op), Plant::None));
}

/// A remove the table never sees leaves an entry the model dropped; the oracle must say so.
#[test]
fn the_oracle_catches_a_remove_forgotten() {
    blaze_sim::control(
        Config::cases(300),
        Plant::None,
        Plant::RemoveForgotten,
        "the table holds",
        |tape, plant| agrees(&tape.vec(0..=40, "ops", op), plant),
    );
}
