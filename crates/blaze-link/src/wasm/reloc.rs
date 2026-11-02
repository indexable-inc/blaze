//! The relocations of wasm objects (`R_WASM_*`, the tool conventions' Linking.md), and how
//! the writer applies them to a placed image.
//!
//! The core hands [`apply`] a function relocation's target already numbered
//! ([`crate::model::Plan::indexed_code`]): the function's index for `FUNCTION_INDEX_*`, its
//! slot in the table (the index + 1; 0 is null) for `TABLE_INDEX_*`. In memory an address is
//! an address.

use super::leb::{padded_sleb, padded_uleb};

pub(super) const FUNCTION_INDEX_LEB: u32 = 0;
pub(super) const TABLE_INDEX_SLEB: u32 = 1;
pub(super) const TABLE_INDEX_I32: u32 = 2;
pub(super) const MEMORY_ADDR_LEB: u32 = 3;
pub(super) const MEMORY_ADDR_SLEB: u32 = 4;
pub(super) const MEMORY_ADDR_I32: u32 = 5;
pub(super) const TYPE_INDEX_LEB: u32 = 6;
pub(super) const GLOBAL_INDEX_LEB: u32 = 7;
pub(super) const FUNCTION_OFFSET_I32: u32 = 8;
pub(super) const SECTION_OFFSET_I32: u32 = 9;
pub(super) const GLOBAL_INDEX_I32: u32 = 13;
pub(super) const TABLE_NUMBER_LEB: u32 = 20;
pub(super) const FUNCTION_INDEX_I32: u32 = 26;

/// The name of a relocation type the tool conventions define.
pub(super) fn name(r_type: u32) -> Option<&'static str> {
    Some(match r_type {
        0 => "R_WASM_FUNCTION_INDEX_LEB",
        1 => "R_WASM_TABLE_INDEX_SLEB",
        2 => "R_WASM_TABLE_INDEX_I32",
        3 => "R_WASM_MEMORY_ADDR_LEB",
        4 => "R_WASM_MEMORY_ADDR_SLEB",
        5 => "R_WASM_MEMORY_ADDR_I32",
        6 => "R_WASM_TYPE_INDEX_LEB",
        7 => "R_WASM_GLOBAL_INDEX_LEB",
        8 => "R_WASM_FUNCTION_OFFSET_I32",
        9 => "R_WASM_SECTION_OFFSET_I32",
        10 => "R_WASM_TAG_INDEX_LEB",
        11 => "R_WASM_MEMORY_ADDR_REL_SLEB",
        12 => "R_WASM_TABLE_INDEX_REL_SLEB",
        13 => "R_WASM_GLOBAL_INDEX_I32",
        14 => "R_WASM_MEMORY_ADDR_LEB64",
        15 => "R_WASM_MEMORY_ADDR_SLEB64",
        16 => "R_WASM_MEMORY_ADDR_I64",
        17 => "R_WASM_MEMORY_ADDR_REL_SLEB64",
        18 => "R_WASM_TABLE_INDEX_SLEB64",
        19 => "R_WASM_TABLE_INDEX_I64",
        20 => "R_WASM_TABLE_NUMBER_LEB",
        21 => "R_WASM_MEMORY_ADDR_TLS_SLEB",
        22 => "R_WASM_FUNCTION_OFFSET_I64",
        23 => "R_WASM_MEMORY_ADDR_LOCREL_I32",
        24 => "R_WASM_TABLE_INDEX_REL_SLEB64",
        25 => "R_WASM_MEMORY_ADDR_TLS_SLEB64",
        26 => "R_WASM_FUNCTION_INDEX_I32",
        _ => return None,
    })
}

/// Whether an entry of a `reloc.*` section of this type has an addend.
pub(super) fn has_addend(r_type: u32) -> bool {
    matches!(r_type, 3 | 4 | 5 | 8 | 9 | 11 | 14 | 15 | 16 | 17 | 21 | 22 | 23 | 25)
}

/// What a relocation type of an object is to the reader: applied by [`apply`], a type index
/// the writer fills from the function's [`blaze_codegen::wasm::Code`], or only in debug
/// sections, which a link drops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Applied,
    Type,
    Debug,
}

/// How the reader takes a relocation type, or why it can't: PIC, TLS (shared memory) and
/// memory64 relocations, which objects for wasm32-wasip1 and wasm32-unknown-unknown don't have.
pub(super) fn kind(r_type: u32) -> Result<Kind, String> {
    match r_type {
        FUNCTION_INDEX_LEB | FUNCTION_INDEX_I32 | TABLE_INDEX_SLEB | TABLE_INDEX_I32
        | MEMORY_ADDR_LEB | MEMORY_ADDR_SLEB | MEMORY_ADDR_I32 | GLOBAL_INDEX_LEB
        | GLOBAL_INDEX_I32 | TABLE_NUMBER_LEB => Ok(Kind::Applied),
        TYPE_INDEX_LEB => Ok(Kind::Type),
        FUNCTION_OFFSET_I32 | SECTION_OFFSET_I32 => Ok(Kind::Debug),
        _ => Err(match name(r_type) {
            // alloc: error path
            Some(name) => format!(
                "the relocation {name}: position-independent, thread-local (shared memory) and \
                 memory64 code is not linked for wasm32-wasip1 or wasm32-unknown-unknown"
            ),
            // alloc: error path
            None => format!("an unknown relocation type {r_type}"),
        }),
    }
}

/// Writes relocation `r_type` at the start of `bytes`, for the target at `s` with `addend`.
pub(super) fn apply(r_type: u32, bytes: &mut [u8], s: u64, addend: i64) -> Result<(), String> {
    let what = || name(r_type).unwrap_or("an unknown wasm relocation");
    let no_addend = || match addend {
        0 => Ok(()),
        // alloc: error path
        _ => Err(format!("{}: an addend of {addend} to a function or an index", what())),
    };
    let index = |value: u64| {
        // alloc: error path
        u32::try_from(value).map_err(|_| format!("{}: the index {value} is past 32 bits", what()))
    };
    let address = || {
        let value = i128::from(s) + i128::from(addend);
        u32::try_from(value)
            // alloc: error path
            .map_err(|_| format!("{}: the address {value:#x} is outside 32-bit memory", what()))
    };
    match r_type {
        FUNCTION_INDEX_LEB | FUNCTION_INDEX_I32 => {
            no_addend()?;
            let function = index(s)?;
            match r_type {
                FUNCTION_INDEX_LEB => put(bytes, &padded_uleb(function), what()),
                _ => put(bytes, &function.to_le_bytes(), what()),
            }
        }
        TABLE_INDEX_SLEB => {
            no_addend()?;
            let slot = i32::try_from(s)
                // alloc: error path
                .map_err(|_| format!("{}: the table slot {s} is past 31 bits", what()))?;
            put(bytes, &padded_sleb(slot), what())
        }
        TABLE_INDEX_I32 => {
            no_addend()?;
            put(bytes, &index(s)?.to_le_bytes(), what())
        }
        MEMORY_ADDR_LEB => put(bytes, &padded_uleb(address()?), what()),
        // `i32.const` takes the address as a signed number: one past 2 GiB is negative.
        MEMORY_ADDR_SLEB => put(bytes, &padded_sleb(address()? as i32), what()),
        MEMORY_ADDR_I32 => put(bytes, &address()?.to_le_bytes(), what()),
        GLOBAL_INDEX_LEB | TABLE_NUMBER_LEB => {
            no_addend()?;
            put(bytes, &padded_uleb(index(s)?), what())
        }
        GLOBAL_INDEX_I32 => {
            no_addend()?;
            put(bytes, &index(s)?.to_le_bytes(), what())
        }
        // alloc: error path
        TYPE_INDEX_LEB => Err(format!(
            "{}: a type index is the writer's to fill from the function's code header, not a \
             relocation of the core's",
            what()
        )),
        // alloc: error path
        FUNCTION_OFFSET_I32 | SECTION_OFFSET_I32 => Err(format!(
            "{}: an offset into the code or a section, which only debug sections have, and a \
             wasm link drops them",
            what()
        )),
        // alloc: error path
        _ => Err(kind(r_type).err().unwrap_or_else(|| format!("{}: not applied", what()))),
    }
}

fn put(bytes: &mut [u8], value: &[u8], what: &str) -> Result<(), String> {
    let left = bytes.len();
    let place = bytes
        .get_mut(..value.len())
        // alloc: error path
        .ok_or_else(|| format!("{what}: {} bytes to write and {left} left", value.len()))?;
    place.copy_from_slice(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(r_type: u32, s: u64, addend: i64, len: usize) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0xaa; len + 2];
        apply(r_type, &mut bytes, s, addend)?;
        assert_eq!(&bytes[len..], [0xaa, 0xaa], "wrote past the place");
        bytes.truncate(len);
        Ok(bytes)
    }

    #[test]
    fn function_indices() {
        assert_eq!(applied(FUNCTION_INDEX_LEB, 5, 0, 5).unwrap(), [0x85, 0x80, 0x80, 0x80, 0]);
        assert_eq!(applied(FUNCTION_INDEX_LEB, 0, 0, 5).unwrap(), [0x80, 0x80, 0x80, 0x80, 0]);
        assert_eq!(applied(FUNCTION_INDEX_I32, 5, 0, 4).unwrap(), [5, 0, 0, 0]);
        assert!(applied(FUNCTION_INDEX_LEB, 6, 1, 5).is_err());
        assert!(applied(FUNCTION_INDEX_LEB, 1 << 33, 0, 5).is_err());
    }

    #[test]
    fn table_slots() {
        // A function's address is its slot; 0 is null.
        assert_eq!(applied(TABLE_INDEX_SLEB, 3, 0, 5).unwrap(), [0x83, 0x80, 0x80, 0x80, 0]);
        assert_eq!(applied(TABLE_INDEX_SLEB, 0, 0, 5).unwrap(), [0x80, 0x80, 0x80, 0x80, 0]);
        assert_eq!(applied(TABLE_INDEX_SLEB, 64, 0, 5).unwrap(), [0xc0, 0x80, 0x80, 0x80, 0]);
        assert_eq!(applied(TABLE_INDEX_I32, 300, 0, 4).unwrap(), [44, 1, 0, 0]);
        assert!(applied(TABLE_INDEX_SLEB, 1 << 31, 0, 5).is_err());
    }

    #[test]
    fn memory_addresses() {
        let at = 0x10_0000;
        assert_eq!(applied(MEMORY_ADDR_SLEB, at, 8, 5).unwrap(), [0x88, 0x80, 0xc0, 0x80, 0]);
        assert_eq!(applied(MEMORY_ADDR_LEB, at, 8, 5).unwrap(), [0x88, 0x80, 0xc0, 0x80, 0]);
        assert_eq!(applied(MEMORY_ADDR_I32, at, -8, 4).unwrap(), [0xf8, 0xff, 0x0f, 0]);
        // Past 2 GiB the signed form is negative, the unsigned one is not.
        let high = 0x8000_0000;
        assert_eq!(applied(MEMORY_ADDR_SLEB, high, 0, 5).unwrap(), padded_sleb(i32::MIN));
        assert_eq!(applied(MEMORY_ADDR_LEB, high, 0, 5).unwrap(), padded_uleb(0x8000_0000));
        assert!(applied(MEMORY_ADDR_I32, u64::from(u32::MAX), 1, 4).is_err());
        assert!(applied(MEMORY_ADDR_I32, 4, -5, 4).is_err());
    }

    #[test]
    fn globals_and_tables() {
        assert_eq!(applied(GLOBAL_INDEX_LEB, 0, 0, 5).unwrap(), [0x80, 0x80, 0x80, 0x80, 0]);
        assert_eq!(applied(GLOBAL_INDEX_I32, 2, 0, 4).unwrap(), [2, 0, 0, 0]);
        assert_eq!(applied(TABLE_NUMBER_LEB, 0, 0, 5).unwrap(), [0x80, 0x80, 0x80, 0x80, 0]);
    }

    #[test]
    fn refused() {
        assert!(applied(TYPE_INDEX_LEB, 0, 0, 5).is_err());
        assert!(applied(FUNCTION_OFFSET_I32, 0, 0, 4).is_err());
        assert!(applied(SECTION_OFFSET_I32, 0, 0, 4).is_err());
        let tls = applied(21, 0, 0, 5).unwrap_err();
        assert!(tls.contains("R_WASM_MEMORY_ADDR_TLS_SLEB"), "{tls}");
        let unknown = applied(99, 0, 0, 5).unwrap_err();
        assert!(unknown.contains("99"), "{unknown}");
        let mut short = [0; 3];
        assert!(apply(MEMORY_ADDR_SLEB, &mut short, 0, 0).is_err());
    }

    #[test]
    fn applying_allocates_nothing_per_relocation() {
        // Every applied type, 1000 times each, in place in one image.
        let types = [
            (FUNCTION_INDEX_LEB, 0),
            (FUNCTION_INDEX_I32, 0),
            (TABLE_INDEX_SLEB, 0),
            (TABLE_INDEX_I32, 0),
            (MEMORY_ADDR_LEB, 8),
            (MEMORY_ADDR_SLEB, -8),
            (MEMORY_ADDR_I32, 4),
            (GLOBAL_INDEX_LEB, 0),
            (GLOBAL_INDEX_I32, 0),
            (TABLE_NUMBER_LEB, 0),
        ];
        let mut image = vec![0; 1000 * types.len() * 5];
        let applied = blaze_alloc_count::assert_at_most(0, "relocations applied", || {
            let mut places = image.chunks_exact_mut(5);
            let mut applied = 0;
            for at in 0..1000u64 {
                for &(r_type, addend) in &types {
                    let place = places.next().expect("a place per relocation");
                    applied += usize::from(apply(r_type, place, 0x10_0000 + at, addend).is_ok());
                }
            }
            applied
        });
        assert_eq!(applied, 1000 * types.len());
        assert_eq!(image[..5], padded_uleb(0x10_0000));
    }

    #[test]
    fn kinds() {
        assert_eq!(kind(MEMORY_ADDR_SLEB), Ok(Kind::Applied));
        assert_eq!(kind(TYPE_INDEX_LEB), Ok(Kind::Type));
        assert_eq!(kind(SECTION_OFFSET_I32), Ok(Kind::Debug));
        assert!(kind(14).is_err());
        assert!(kind(200).is_err());
        assert!(has_addend(MEMORY_ADDR_I32) && !has_addend(TABLE_INDEX_I32));
    }
}
