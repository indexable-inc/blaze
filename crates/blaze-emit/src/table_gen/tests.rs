//! The generator's tests (T1): every template, filled with operand values, is the bytes enc.rs
//! makes for those values; a planted generator that drops `from_bit` is caught; the table is
//! deterministic and the committed one is what the generator makes; the reader refuses rows out
//! of order.

use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::{Broke, Config, Tape};
use blaze_tier::table::ROW_BYTES;
use blaze_tier::{Arch, Machine, Os, Table, TableError};

use super::{Code, EncodedTable, HEAD_BYTES, IMM_CLASSES, IRREGULAR, MOST_BYTES, OPERANDS, Plant, Recipe, Vals};
use super::{arm64, fill, generate, generate_planted};

const MACHINE: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: 0 };

fn table(plant: Plant) -> AVec<'static, u8> {
    let mut out = Vec::new_in(Alloc::global());
    if let Err(error) = generate_planted(MACHINE, &mut out, Alloc::global(), plant) {
        panic!("the arm64 table was not generated: {error}");
    }
    out
}

const fn mask(bits: u8) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

/// The template of `recipe` filled with `vals` against the recipe's own encoder: `None` when
/// equal, else what differs.
fn differs(table: &EncodedTable<'_>, recipe: &Recipe<'_>, vals: &Vals) -> Option<String> {
    let mut want = Code::new();
    (recipe.encode)(vals, &mut want);
    let Some(template) = table.find(&recipe.pattern) else {
        return Some(format!("no template for {:?}", recipe.pattern));
    };
    let len = template.bytes.len();
    if len != want.bytes().len() || len > MOST_BYTES {
        return Some(format!("{:?}: {len} template bytes, the encoder {}", recipe.pattern, want.bytes().len()));
    }
    let mut got = [0u8; MOST_BYTES];
    got[..len].copy_from_slice(template.bytes);
    fill(&mut got[..len], template.fields, vals);
    match got[..len] == *want.bytes() {
        true => None,
        false => Some(format!(
            "{:?} with {vals:x?}: template filled {:02x?}, enc.rs {:02x?}",
            recipe.pattern,
            &got[..len],
            want.bytes()
        )),
    }
}

/// Every recipe with random values of its operands' widths (registers 0..32, immediates and
/// displacements of their class's bits) agrees with the table.
fn agrees(table: &EncodedTable<'_>, tape: &Tape) -> Result<(), Broke> {
    let mut first: Option<String> = None;
    let walked = arm64::recipes(&mut |recipe| {
        let mut vals = [0u64; OPERANDS];
        for slot in recipe.slots {
            vals[usize::from(slot.operand)] = tape.next_u64("an operand's value") & mask(slot.bits);
        }
        if first.is_none() {
            first = differs(table, recipe, &vals);
        }
        Ok(())
    });
    if let Err(error) = walked {
        return Err(Broke::Harness(format!("the recipes stopped: {error}")));
    }
    match first {
        None => Ok(()),
        Some(why) => Err(Broke::Oracle(why)),
    }
}

/// Every template filled with random operands equals enc.rs's bytes for them.
/// Expected: passes. Fails if a field is found at the wrong bits, a field is missed (its bits
/// stay zero) or the template's bytes are not cleared where fields are.
#[test]
fn templates_fill_to_the_encoders_bytes() {
    let bytes = table(Plant::None);
    let table = match EncodedTable::read(&bytes, Alloc::global()) {
        Ok(table) => table,
        Err(error) => panic!("the generated table does not read back: {error}"),
    };
    blaze_sim::check(Config::cases(256), |tape: &Tape| agrees(&table, tape));
}

/// The control (rule 8): a generator that drops each field's `from_bit` is caught: the 64-bit
/// constant's four `movz`/`movk` fields hold bits 16.., 32.. and 48.. of one operand.
/// Expected: prints "plant DropFromBit: caught at recipe k of n" and passes.
#[test]
fn plant_drop_from_bit_is_caught() {
    let bytes = table(Plant::DropFromBit);
    let table = match EncodedTable::read(&bytes, Alloc::global()) {
        Ok(table) => table,
        Err(error) => panic!("the planted table does not read back: {error}"),
    };
    let (mut n, mut caught) = (0usize, None);
    let walked = arm64::recipes(&mut |recipe| {
        let mut vals = [0u64; OPERANDS];
        for slot in recipe.slots {
            vals[usize::from(slot.operand)] = IRREGULAR & mask(slot.bits);
        }
        if caught.is_none() && differs(&table, recipe, &vals).is_some() {
            caught = Some(n);
        }
        n += 1;
        Ok(())
    });
    assert!(walked.is_ok(), "the recipes stopped");
    match caught {
        Some(k) => std::println!("plant DropFromBit: caught at recipe {k} of {n}"),
        None => panic!("plant DropFromBit: not caught in {n} recipes"),
    }
}

/// Generating twice gives equal bytes and equal heads (rule 25).
/// Expected: passes. Fails if any order in the generator depends on anything but the recipes.
#[test]
fn generating_twice_gives_equal_bytes() {
    let (mut a, mut b) = (Vec::new_in(Alloc::global()), Vec::new_in(Alloc::global()));
    let head_a = generate(MACHINE, &mut a, Alloc::global());
    let head_b = generate(MACHINE, &mut b, Alloc::global());
    assert_eq!(head_a, head_b);
    assert!(a[..] == b[..], "two runs of the generator wrote different bytes");
    let head = match head_a {
        Ok(head) => head,
        Err(error) => panic!("{error}"),
    };
    assert_eq!(head.digest, blaze_blake3::hash(&a));
}

/// The committed table (crates/blaze-emit/table/arm64.bztab, the bytes build.rs pins) is what the
/// generator makes from today's enc.rs.
/// Expected: passes. Fails after a change to enc.rs or the recipes until the table is made again:
/// `cargo run -p blaze-emit --example gen_table` from the workspace's root.
#[test]
fn the_committed_table_is_the_generators() {
    let committed: &[u8] = include_bytes!("../../table/arm64.bztab");
    let made = table(Plant::None);
    assert!(
        committed == &made[..],
        "crates/blaze-emit/table/arm64.bztab is not what the generator makes: run `cargo run -p blaze-emit --example gen_table` from the workspace's root"
    );
}

/// Rows out of order are refused, naming the row.
/// Expected: passes. Fails if the reader stops checking the order its binary search needs.
#[test]
fn read_refuses_rows_out_of_order() {
    let mut bytes = table(Plant::None);
    let (first, second) = (HEAD_BYTES, HEAD_BYTES + ROW_BYTES);
    let mut row = [0u8; ROW_BYTES];
    row.copy_from_slice(&bytes[first..second]);
    bytes.copy_within(second..second + ROW_BYTES, first);
    bytes[second..second + ROW_BYTES].copy_from_slice(&row);
    match EncodedTable::read(&bytes, Alloc::global()) {
        Err(error) => assert_eq!(error, TableError::Unsorted { row: 1 }),
        Ok(_) => panic!("rows out of order were read"),
    }
}

/// The reader's class table is in `ImmClass`'s byte order, which the writer writes.
/// Expected: passes. Fails if a class is added to table.rs and not here, or out of place.
#[test]
fn imm_classes_are_in_byte_order() {
    for (at, class) in IMM_CLASSES.into_iter().enumerate() {
        assert_eq!(usize::from(class as u8), at, "{class:?}");
    }
}
