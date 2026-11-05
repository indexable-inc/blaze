//! A function's `.gcc_except_table` entry, written in one pass into a buffer of its exact
//! size. Every entry blaze writes has one shape: one type, which catches everything, one
//! action that catches it, and a row for each call. Those are the bytes rustc's Cranelift
//! backend writes for it with its tables of any shape (`gcc_except_table`, which the tests
//! compare with).

use blaze_format::leb::write_u64 as uleb;
use cranelift_codegen::gimli::{DW_EH_PE_omit, DW_EH_PE_udata4, DW_EH_PE_uleb128};

/// A row of the call-site table: the call that returns to `start + 1`, where it lands when it
/// unwinds (0 for nowhere: the unwind goes on to the caller), and whether the pad catches.
/// 12 bytes; rows are made from Cranelift's call sites as they are written and never kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CallEntry {
    pub(super) start: u32,
    pub(super) landing_pad: u32,
    pub(super) catch: bool,
}

const _: () = assert!(size_of::<CallEntry>() == 12);

/// The action table: the one action, which catches type 1 and has no next action.
const ACTIONS: [u8; 2] = [1, 0];
/// The type table: the one type, 4 bytes of 0, which is any exception.
const TYPES: [u8; 4] = [0; 4];
/// The encodings of the landing pads' start (none: the function's) and of the types.
const HEADER: [u8; 2] = [DW_EH_PE_omit.0, DW_EH_PE_udata4.0];

/// The entry of the calls `rows` gives, the same each time it is called: once to size the
/// entry and once to write it.
pub(super) fn write<I: Iterator<Item = CallEntry>>(rows: impl Fn() -> I) -> Vec<u8> {
    let table: u64 = rows().map(row_size).sum();
    let call_sites = 1 + uleb_size(table) + table;
    let (header, actions) = (HEADER.len() as u64, ACTIONS.len() as u64);
    let tail = actions + TYPES.len() as u64;
    // The types start at a multiple of 4, which padding after the actions makes them. The
    // offset of their end, after the header, counts the padding and is as long as it is.
    let mut padding = 0;
    while !(header + uleb_size(call_sites + tail + padding) + call_sites + actions + padding)
        .is_multiple_of(4)
    {
        padding += 1;
    }
    let to_types = call_sites + tail + padding;
    let len = header + uleb_size(to_types) + to_types;
    let mut out = Vec::with_capacity(len as usize);
    out.extend_from_slice(&HEADER);
    uleb(&mut out, to_types);
    out.push(DW_EH_PE_uleb128.0);
    uleb(&mut out, table);
    for row in rows() {
        uleb(&mut out, row.start.into());
        out.push(1);
        uleb(&mut out, row.landing_pad.into());
        out.push(u8::from(row.catch));
    }
    out.extend_from_slice(&ACTIONS);
    out.resize(out.len() + padding as usize, 0);
    out.extend_from_slice(&TYPES);
    debug_assert_eq!(out.len() as u64, len);
    out
}

/// A row's bytes: where the call starts, its length of 1, its pad, and its action.
fn row_size(row: CallEntry) -> u64 {
    uleb_size(row.start.into()) + 1 + uleb_size(row.landing_pad.into()) + 1
}

fn uleb_size(value: u64) -> u64 {
    blaze_format::leb::len_u64(value) as u64
}

#[cfg(test)]
mod tests {
    use cranelift_codegen::gimli::{
        self, Encoding, Format, RunTimeEndian,
        write::{Address, EndianVec},
    };

    use super::super::gcc_except_table::{
        Action, ActionKind, ActionTable, CallSite, CallSiteTable, GccExceptTable, TypeInfoTable,
    };
    use super::*;

    /// The entry rustc's Cranelift backend writes for `rows`.
    fn model(rows: &[CallEntry], address_size: u8) -> Vec<u8> {
        let mut table = GccExceptTable {
            call_sites: CallSiteTable(vec![]),
            actions: ActionTable::new(),
            type_info: TypeInfoTable::new(gimli::DW_EH_PE_udata4),
        };
        let catch_type = table.type_info.add(Address::Constant(0));
        let catch_action =
            table.actions.add(Action { kind: ActionKind::Catch(catch_type), next_action: None });
        for row in rows {
            table.call_sites.0.push(CallSite {
                start: row.start.into(),
                length: 1,
                landing_pad: row.landing_pad.into(),
                action_entry: row.catch.then_some(catch_action),
            });
        }
        let encoding = Encoding { format: Format::Dwarf32, version: 1, address_size };
        let mut writer = EndianVec::new(RunTimeEndian::Little);
        table.write(&mut writer, encoding).expect("the model writes");
        writer.into_vec()
    }

    /// A seeded stream of rows, with offsets across every length of LEB128 a `u32` has.
    fn rows(seed: u64, count: usize) -> Vec<CallEntry> {
        let mut rng = blaze_ids::rng::SplitMix64::new(seed);
        let mut next = move || rng.next_u64();
        let mut start = 0u32;
        (0..count)
            .map(|_| {
                let bits = next();
                let step = (bits as u32) >> (bits >> 59);
                start = start.saturating_add(step % 0x40_0000 + 1);
                let landing_pad = match bits >> 32 & 3 {
                    0 => 0,
                    _ => ((bits >> 34) as u32) >> ((bits >> 40) & 31),
                };
                CallEntry { start, landing_pad, catch: landing_pad != 0 && bits >> 63 == 1 }
            })
            .collect()
    }

    #[test]
    fn writes_what_the_tables_of_any_shape_write() {
        for seed in 0..500u64 {
            let rows = rows(seed, (seed % 97) as usize);
            for address_size in [4, 8] {
                let ours = write(|| rows.iter().copied());
                assert_eq!(ours, model(&rows, address_size), "seed {seed}");
                assert_eq!(ours.capacity(), ours.len(), "seed {seed}");
            }
        }
    }

    #[test]
    fn every_padding_a_header_can_need_is_written_alike() {
        // Tables whose lengths cross the 127 and 16 383 bytes where the offset of the types
        // takes one more byte, and so another padding.
        for count in 0..40 {
            for start in [0, 1, 0x7f, 0x80, 0x3fff, 0x4000, 0x1f_ffff, 0x20_0000, u32::MAX - 1] {
                let rows: Vec<_> = (0..count)
                    .map(|i| CallEntry {
                        start: start.saturating_add(i),
                        landing_pad: i,
                        catch: i % 3 == 0,
                    })
                    .collect();
                assert_eq!(write(|| rows.iter().copied()), model(&rows, 8), "{count} at {start}");
            }
        }
    }

    #[test]
    fn leb128_sizes_are_what_is_written() {
        for value in [0, 1, 0x7f, 0x80, 0x3fff, 0x4000, u64::from(u32::MAX), u64::MAX] {
            let mut ours = Vec::new();
            uleb(&mut ours, value);
            assert_eq!(ours.len() as u64, uleb_size(value), "{value:#x}");
            let mut theirs = Vec::new();
            gimli::leb128::write::unsigned(&mut theirs, value).expect("written");
            assert_eq!(ours, theirs, "{value:#x}");
        }
    }
}
