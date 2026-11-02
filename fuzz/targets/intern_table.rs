//! An interner table's section (`crates/intern/src/table.rs`, `Table::restore`, under
//! `intern::restore_symbols` and `Mapped::restore`): its root, then every column checked by
//! `Table::validate` against the others, then its values read in place, a text table's handed to
//! `str::from_utf8_unchecked` (`symbol.rs`, `mod text`). Fed through `intern::fuzz`, which
//! restores into a fresh table of a small text shape, never the process's.
//!
//! The input is the section's bytes. The hook puts them in an image of their own, so every CRC
//! is good and the checks after it are what stands between the bytes and a read. Those leave the
//! index's reach to the CRC (a value the index misses costs a second ID, never a wrong value), so
//! `find` of a restored value may miss it; in a table its writer wrote, it may not.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - restoring never panics, on any bytes, and asks for at most `blaze_fuzz::allowed(len)` bytes
//!   at once; a refusal is `Invalid` naming a byte of the section, or `Corrupt` by a column's
//!   place (bounds or alignment, never a CRC);
//! - on `Ok`, every ID below the table's end reads as a value or as freed: the values' IDs are
//!   ascending and below the end, every compiled-in ID among them, and every value is UTF-8;
//!   `find` of a value is no ID or one holding the same bytes;
//! - round trip: the table written again with every value's ID live restores to the same end
//!   and the same IDs and bytes (so the same freed IDs), where `find` of each value is the first
//!   ID holding it (its own when no other holds it), and writing that gives the same bytes.

#![forbid(unsafe_code)]

use std::{collections::HashMap, time::Duration};

use blaze_fuzzer::Target;
use blaze_ids::rng::SplitMix64;
use blaze_image::Cause;
use intern::{
    RestoreError,
    fuzz::{self, Restored},
};

const TARGET: Target = Target {
    name: "intern_table",
    check,
    seeds,
    dictionary: &[
        &[0xff, 0xff, 0xff, 0xff],
        &[0, 0, 0, 0],
        &[1, 0, 0, 0],
        &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80],
        &[0xff],
        "é".as_bytes(),
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

/// Byte offsets in the section of three fields of `table.rs`'s `Root` (`#[repr(C)]`, eight
/// `u64`s then `u32`s): copied here because the crate keeps `Root` to itself; the tests check
/// the copy.
const ROOT_STARTS: usize = 0;
const ROOT_BYTES: usize = 48;
const ROOT_FIRST: usize = 64;

/// Values of the freed seed: the second and fourth are freed.
const VALUES: [&[u8]; 4] = [b"kept", b"dropped", b"also kept", b"dropped too"];

fn check(data: &[u8]) {
    let restored = match restore(data) {
        Ok(restored) => restored,
        Err(RestoreError::Invalid { what, at, .. }) => {
            assert!(at <= data.len() as u64, "{what}: refused at byte {at} of a section of {} bytes", data.len());
            return;
        }
        Err(RestoreError::Corrupt(corrupt)) => {
            assert!(
                matches!(corrupt.cause, Cause::Bounds { .. } | Cause::Misaligned { .. }),
                "a section of good CRCs is corrupt only by a column's place: {corrupt}"
            );
            return;
        }
        Err(other) => {
            panic!("a fresh table restoring a section of its own schema is refused only as invalid or corrupt: {other}")
        }
    };
    readable(&restored);
    for ((id, bytes), found) in restored.values.iter().zip(&restored.found) {
        if let Some(found) = *found {
            assert_eq!(
                bytes_of(&restored, found),
                Some(&**bytes),
                "find of ID {id}'s bytes gives ID {found}, which holds other bytes"
            );
        }
    }

    let again = restore(&restored.written)
        .unwrap_or_else(|error| panic!("the restored table, written again, restores: {error}"));
    readable(&again);
    assert_eq!(
        (again.first, again.end),
        (restored.first, restored.end),
        "the table written again has the same compiled-in IDs and end"
    );
    assert_eq!(again.values, restored.values, "the table written again holds the same IDs and bytes");
    let mut first_of: HashMap<&[u8], u32> = HashMap::with_capacity(again.values.len());
    for (id, bytes) in &again.values {
        first_of.entry(&**bytes).or_insert(*id);
    }
    for ((id, bytes), found) in again.values.iter().zip(&again.found) {
        assert_eq!(
            *found,
            first_of.get(&**bytes).copied(),
            "in a table its writer wrote, find of ID {id}'s bytes is the first ID holding them"
        );
    }
    assert_eq!(again.written, restored.written, "writing one table twice gives the same bytes");
}

/// `fuzz::restore` of `section`, held to `blaze_fuzz::allowed` of its length.
fn restore(section: &[u8]) -> Result<Restored, RestoreError> {
    let (largest, restored) = blaze_fuzz::largest_allocation(|| fuzz::restore(section));
    let allowed = blaze_fuzz::allowed(section.len());
    assert!(
        largest <= allowed,
        "restoring a section of {} bytes asks for at most {allowed} at once, not {largest}",
        section.len()
    );
    restored
}

/// Checks that `restored` reads as a table: IDs ascending and below its end, every compiled-in
/// one there, every value text.
fn readable(restored: &Restored) {
    let Restored { first, end, values, found, .. } = restored;
    assert!(1 <= *first && first <= end, "compiled-in IDs 1..{first} below the end {end}");
    assert_eq!(values.len(), found.len(), "one find per value");
    assert!(values.windows(2).all(|pair| pair[0].0 < pair[1].0), "the values' IDs ascend, each once");
    assert!(values.first().is_none_or(|(id, _)| *id >= 1), "ID 0 names no value");
    assert!(values.last().is_none_or(|(id, _)| id < end), "every value's ID is below the end {end}");
    let statics = values.iter().take_while(|(id, _)| id < first).count();
    assert_eq!(statics, (*first - 1) as usize, "every compiled-in ID reads");
    for (id, bytes) in values {
        assert!(std::str::from_utf8(bytes).is_ok(), "ID {id} of a text table holds {bytes:?}, not UTF-8");
    }
}

/// The bytes of `id` in `restored`, `None` when it is freed.
fn bytes_of(restored: &Restored, id: u32) -> Option<&[u8]> {
    let at = restored.values.binary_search_by_key(&id, |(id, _)| *id).ok()?;
    Some(&restored.values[at].1)
}

/// The first ID of `restored` holding `value`.
fn id_of(restored: &Restored, value: &[u8]) -> Option<u32> {
    restored.values.iter().find(|(_, bytes)| **bytes == *value).map(|(id, _)| *id)
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

/// 200 short names from `a`, `b`, `é`, `_` and `0`, as `table/tests.rs`'s `name()`: many share
/// an order key, a hash tag or their bytes, and some are a compiled-in value or empty.
fn names() -> Vec<String> {
    let mut rng = SplitMix64::new(0x1d5);
    (0..200)
        .map(|_| {
            let len = rng.index(12);
            (0..len).map(|_| ['a', 'b', 'é', '_', '0'][rng.index(5)]).collect()
        })
        .collect()
}

/// [`VALUES`] written with the second and fourth freed, and their IDs.
fn with_freed() -> (Vec<u8>, Vec<u32>) {
    let all = fuzz::restore(&fuzz::section(&VALUES, &[])).expect("a section `section` wrote restores");
    let freed: Vec<u32> = [VALUES[1], VALUES[3]]
        .iter()
        .map(|value| id_of(&all, value).expect("every value written has an ID"))
        .collect();
    (fuzz::section(&VALUES, &freed), freed)
}

/// Three values with the first's end overwritten by `u32::MAX`, past the values; and that
/// start's byte.
fn past_the_values() -> (Vec<u8>, u64) {
    let mut bytes = fuzz::section(&[&b"first value"[..], b"second", b"third"], &[]);
    let at = word(&bytes, ROOT_STARTS) + 4;
    bytes[at as usize..at as usize + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    (bytes, at)
}

/// One value whose first byte is `0xff`, which no UTF-8 begins with; and that byte.
fn not_text() -> (Vec<u8>, u64) {
    let mut bytes = fuzz::section(&[&b"text"[..]], &[]);
    let at = word(&bytes, ROOT_BYTES);
    bytes[at as usize] = 0xff;
    (bytes, at)
}

/// No bytes; the compiled-in values alone; 200 names; values with IDs freed; one value of
/// 64 KiB; and the two damaged sections `table/tests.rs` refuses (a start past the values, a
/// value that is not UTF-8).
fn seeds() -> Vec<Vec<u8>> {
    let names = names();
    let names: Vec<&[u8]> = names.iter().map(|name| name.as_bytes()).collect();
    let long = vec![b'x'; 1 << 16];
    vec![
        Vec::new(),
        fuzz::section(&[], &[]),
        fuzz::section(&names, &[]),
        with_freed().0,
        fuzz::section(&[long.as_slice()], &[]),
        past_the_values().0,
        not_text().0,
    ]
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&TARGET);
    }

    /// Each seed `section` wrote restores to what it was given, and the copy of `Root`'s
    /// offsets agrees with the table's.
    #[test]
    fn written_sections_restore_as_written() {
        let names = names();
        let bytes: Vec<&[u8]> = names.iter().map(|name| name.as_bytes()).collect();
        let section = fuzz::section(&bytes, &[]);
        let restored = fuzz::restore(&section).expect("a section `section` wrote restores");
        let first = u32::from_le_bytes(section[ROOT_FIRST..ROOT_FIRST + 4].try_into().expect("4 bytes"));
        assert_eq!(first, restored.first, "`Root::first` is at byte {ROOT_FIRST} of the section");
        for name in &names {
            assert!(id_of(&restored, name.as_bytes()).is_some(), "{name:?} was written");
        }

        let (section, freed) = with_freed();
        let restored = fuzz::restore(&section).expect("a section `section` wrote restores");
        for &id in &freed {
            assert!(id >= restored.first && id < restored.end, "freed ID {id} is below the end");
            assert_eq!(bytes_of(&restored, id), None, "ID {id} was freed");
        }
        for value in [VALUES[0], VALUES[2]] {
            assert!(id_of(&restored, value).is_some(), "{value:?} was kept");
        }
        assert_eq!(VALUES.iter().filter(|value| id_of(&restored, value).is_some()).count(), 2, "only two were kept");
    }

    /// The damaged seeds are refused where `table/tests.rs` refuses them: the copy of `Root`'s
    /// `starts` and `bytes` offsets is the table's.
    #[test]
    fn damaged_sections_are_refused_where_they_are() {
        let refused = |bytes: &[u8]| match fuzz::restore(bytes) {
            Err(RestoreError::Invalid { what, at, .. }) => Some((what, at)),
            _ => None,
        };
        let (bytes, at) = past_the_values();
        assert_eq!(refused(&bytes), Some(("a value's bytes out of order or past the values", at)));
        let (bytes, at) = not_text();
        assert_eq!(refused(&bytes), Some(("a value that is not UTF-8", at)));
    }
}
