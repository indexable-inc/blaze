//! `enc`'s properties: every column decodes to what was encoded, whole and one value at a
//! time; the vector unpack equals its scalar twin at every alignment; no bytes, however
//! corrupt, make a read panic or leave the column; and each oracle catches the bug its
//! control plants.

use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::{Broke, Config, Tape};

use super::bits::{self, BLOCK, PAD};
use super::block::{GetCost, Scheme};
use super::ints::{DictUse, IntPolicy, encode_ints};
use super::plant::{self, Plant};
use super::strs::{StrPolicy, StrScheme, encode_strs};
use super::{IdView, IntView, StrView, encode_ids};

fn global() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn oracle(why: String) -> Broke {
    Broke::Oracle(why)
}

/// A column of one of the shapes records hold, and the ends that break codecs.
fn draw_values(tape: &Tape) -> std::vec::Vec<u64> {
    let n = tape.int_in(0usize..=2600, "len");
    let shape = tape.below(8, "shape");
    let width = tape.int_in(0u32..=64, "width");
    let small = |tape: &Tape| match width {
        0 => 0,
        64 => tape.next_u64("value"),
        w => tape.below(1u64 << w, "value"),
    };
    let mut at: u64 = tape.int("start");
    let mut out = std::vec::Vec::with_capacity(n);
    for j in 0..n {
        let j64 = u64::try_from(j).unwrap_or(0);
        let v = match shape {
            0 => tape.int::<u64>("any"),
            1 => small(tape),
            // Sorted or slowly growing: positions, offsets.
            2 => {
                at = at.wrapping_add(small(tape));
                at
            }
            // Runs: token kinds, parents.
            3 => {
                if j == 0 || tape.one_in(40, "run ends") {
                    at = small(tape);
                }
                at
            }
            // A few distinct values far apart: ids.
            4 => {
                const FEW: [u64; 6] = [0, 1, u64::MAX, 1 << 63, 12_345, u64::MAX - 1];
                FEW[tape.index(FEW.len(), "few")]
            }
            // A line with noise.
            5 => at.wrapping_add(j64.wrapping_mul(37)).wrapping_add(tape.below(4, "noise")),
            // Regular second differences.
            6 => {
                at = at.wrapping_add(j64.wrapping_mul(3));
                at
            }
            // Small values with outliers.
            _ => {
                if tape.one_in(100, "outlier") {
                    u64::MAX - tape.below(3, "outlier value")
                } else {
                    small(tape)
                }
            }
        };
        out.push(v);
    }
    out
}

fn policies() -> std::vec::Vec<IntPolicy> {
    let mut all = vec![
        IntPolicy::RECORD,
        IntPolicy::CONSTANT,
        IntPolicy { max_get: GetCost::Block, dict: DictUse::Try, only: None },
        IntPolicy { dict: DictUse::Always, ..IntPolicy::RECORD },
    ];
    for scheme in Scheme::ALL {
        all.push(IntPolicy { max_get: GetCost::Block, dict: DictUse::Never, only: Some(scheme) });
    }
    all
}

fn encode(values: &[u64], policy: IntPolicy) -> Result<AVec<'static, u8>, Broke> {
    let mut out = Vec::new_in(global());
    encode_ints(values, policy, global(), &mut out).map_err(|e| oracle(format!("encode under {policy:?} was refused: {e}")))?;
    Ok(out)
}

/// The column of `values` under `policy`: the same bytes twice, a block decode by vector and
/// by scalar equal to the values, and every `get(i)` equal to `values[i]`.
fn check_ints(values: &[u64], policy: IntPolicy) -> Result<(), Broke> {
    let bytes = encode(values, policy)?;
    if encode(values, policy)?.as_slice() != bytes.as_slice() {
        return Err(oracle(format!("deterministic: two encodes of one column under {policy:?} differ")));
    }
    let view = IntView::open(&bytes).map_err(|e| oracle(format!("round trip under {policy:?}: open refused: {e}")))?;
    if view.len() != values.len() {
        return Err(oracle(format!("round trip under {policy:?}: {} values, not {}", view.len(), values.len())));
    }
    let (mut vector, mut scalar) = ([0u64; BLOCK], [0u64; BLOCK]);
    for b in 0..view.blocks() {
        let n = view.decode_block(b, &mut vector);
        let want = &values[b * BLOCK..(b * BLOCK + BLOCK).min(values.len())];
        if &vector[..n] != want {
            let at = (0..n).find(|&k| vector[k] != want[k]).unwrap_or(0);
            return Err(oracle(format!(
                "round trip under {policy:?}: block {b} value {at} is {}, not {}",
                vector[at], want[at]
            )));
        }
        view.decode_block_scalar(b, &mut scalar);
        if scalar[..n] != vector[..n] {
            return Err(oracle(format!("scalar twin: block {b}'s scalar decode differs under {policy:?}")));
        }
    }
    for (i, &want) in values.iter().enumerate() {
        let got = view.get(i);
        if got != want {
            return Err(oracle(format!("random access under {policy:?}: get({i}) is {got}, not {want}")));
        }
    }
    Ok(())
}

fn ints_round_trip(tape: &Tape) -> Result<(), Broke> {
    let values = draw_values(tape);
    for policy in policies() {
        check_ints(&values, policy)?;
    }
    Ok(())
}

/// The invariant: every encoded integer, string, and id column decodes back to its exact values, vector unpack equals scalar at every alignment, and corrupt bytes are refused or read in bounds.
#[test]
fn integer_columns_decode_to_their_values() {
    blaze_sim::check(Config::cases(300), ints_round_trip);
}

#[test]
fn the_oracle_catches_a_pack_that_drops_a_bit() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || check_ints(&draw_values(tape), IntPolicy::RECORD));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::PackDropsTopBit, "round trip", run);
}

#[test]
fn the_oracle_catches_a_delta_get_one_short() {
    let policy = IntPolicy { max_get: GetCost::Block, dict: DictUse::Never, only: Some(Scheme::Delta) };
    let run = |tape: &Tape, p: Plant| plant::with(p, || check_ints(&draw_values(tape), policy));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::DeltaGetSumsShort, "random access", run);
}

#[test]
fn the_oracle_catches_a_run_search_one_left() {
    let policy = IntPolicy { max_get: GetCost::Block, dict: DictUse::Never, only: Some(Scheme::Rle) };
    let run = |tape: &Tape, p: Plant| plant::with(p, || check_ints(&draw_values(tape), policy));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::RunSearchLeft, "random access", run);
}

#[test]
fn the_oracle_catches_a_dictionary_code_one_off() {
    let policy = IntPolicy { dict: DictUse::Always, ..IntPolicy::RECORD };
    let run = |tape: &Tape, p: Plant| plant::with(p, || check_ints(&draw_values(tape), policy));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::DictCodePlusOne, "round trip", run);
}

/// The vector unpack of `n` values of a drawn width equals the scalar one and the values, the
/// packed bytes starting at each of the 8 byte alignments.
fn twin(tape: &Tape) -> Result<(), Broke> {
    let w = tape.int_in(0u8..=64, "width");
    let n = tape.int_in(0usize..=300, "count");
    let values: std::vec::Vec<u64> = (0..n).map(|_| tape.next_u64("value") & bits::mask(w)).collect();
    let mut packed = Vec::new_in(global());
    bits::pack(n, w, &mut packed, |j| values[j]).map_err(|e| oracle(format!("pack refused: {e}")))?;
    for align in 0..8 {
        let mut buf = std::vec![0u8; align];
        buf.extend_from_slice(&packed);
        buf.extend_from_slice(&[0; PAD]);
        let (mut vector, mut scalar) = (std::vec![0u64; n], std::vec![0u64; n]);
        bits::unpack(&buf[align..], w, &mut vector);
        bits::unpack_scalar(&buf[align..], w, &mut scalar);
        if vector != scalar || scalar != values {
            return Err(oracle(format!("scalar twin: {n} values of {w} bits at alignment {align} unpack differently")));
        }
    }
    Ok(())
}

#[test]
fn the_vector_unpack_equals_its_scalar_twin_at_every_alignment() {
    blaze_sim::check(Config::cases(500), twin);
}

#[test]
fn the_oracle_catches_a_lane_shifted_short() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || twin(tape));
    blaze_sim::control(Config::cases(500), Plant::Clean, Plant::LaneShiftShort, "scalar twin", run);
}

/// Strings that repeat, as names do, with bytes of every value among them.
fn draw_strs(tape: &Tape) -> std::vec::Vec<std::vec::Vec<u8>> {
    let pool: std::vec::Vec<std::vec::Vec<u8>> =
        tape.vec(1..=24, "pool", |tape| tape.text_from("abcdefgh_ijkl::ABC0123", 0..=20, "name").into_bytes());
    let n = tape.int_in(0usize..=500, "rows");
    (0..n)
        .map(|_| match tape.below(4, "row") {
            0 => tape.bytes(0..=12, "bytes"),
            _ => pool[tape.index(pool.len(), "pick")].clone(),
        })
        .collect()
}

fn check_strs(strs: &[std::vec::Vec<u8>], policy: StrPolicy) -> Result<(), Broke> {
    let refs: std::vec::Vec<&[u8]> = strs.iter().map(std::vec::Vec::as_slice).collect();
    let mut bytes = Vec::new_in(global());
    encode_strs(&refs, policy, global(), &mut bytes).map_err(|e| oracle(format!("encode under {policy:?} was refused: {e}")))?;
    let view = StrView::open(&bytes).map_err(|e| oracle(format!("round trip under {policy:?}: open refused: {e}")))?;
    if view.len() != strs.len() {
        return Err(oracle(format!("round trip under {policy:?}: {} strings, not {}", view.len(), strs.len())));
    }
    let mut buf = std::vec::Vec::new();
    for (i, want) in strs.iter().enumerate() {
        buf.resize(view.need(i), 0);
        match view.get(i, &mut buf) {
            Ok(got) if got == want.as_slice() => {}
            Ok(got) => return Err(oracle(format!("round trip under {policy:?}: string {i} is {got:?}, not {want:?}"))),
            Err(e) => return Err(oracle(format!("round trip under {policy:?}: string {i} was refused: {e}"))),
        }
    }
    Ok(())
}

fn strs_round_trip(tape: &Tape) -> Result<(), Broke> {
    let strs = draw_strs(tape);
    check_strs(&strs, StrPolicy::RECORD)?;
    for scheme in StrScheme::ALL {
        check_strs(&strs, StrPolicy { only: Some(scheme), ..StrPolicy::RECORD })?;
    }
    Ok(())
}

#[test]
fn string_columns_decode_to_their_strings() {
    blaze_sim::check(Config::cases(200), strs_round_trip);
}

#[test]
fn the_oracle_catches_an_escape_without_its_byte() {
    let policy = StrPolicy { only: Some(StrScheme::Fsst), ..StrPolicy::RECORD };
    let run = |tape: &Tape, p: Plant| plant::with(p, || check_strs(&draw_strs(tape), policy));
    blaze_sim::control(Config::cases(200), Plant::Clean, Plant::EscapeDropsByte, "round trip", run);
}

/// Reads every value of whatever `open` accepts: a panic here is a read past a check.
fn read_all(bytes: &[u8]) {
    if let Ok(view) = IntView::open(bytes) {
        let mut out = [0u64; BLOCK];
        for b in 0..view.blocks() {
            view.decode_block(b, &mut out);
        }
        for i in 0..view.len() {
            core::hint::black_box(view.get(i));
        }
    }
    if let Ok(view) = StrView::open(bytes) {
        let mut buf = std::vec::Vec::new();
        for i in 0..view.len() {
            buf.resize(view.need(i), 0);
            core::hint::black_box(view.get(i, &mut buf).ok());
        }
    }
    if let Ok(view) = IdView::<4>::open(bytes) {
        for i in 0..view.len() {
            core::hint::black_box(view.get(i));
        }
    }
}

/// Ids of 4 bytes (the layout is `W`-generic; 4 keeps the cases small) drawn from a pool, so
/// rows repeat as a record's references to one item do.
fn draw_ids(tape: &Tape) -> std::vec::Vec<[u8; 4]> {
    let pool: std::vec::Vec<[u8; 4]> = tape.vec(1..=40, "pool", |tape| tape.next_u32("id").to_le_bytes());
    let n = tape.int_in(0usize..=2200, "rows");
    (0..n).map(|_| pool[tape.index(pool.len(), "pick")]).collect()
}

fn check_ids(rows: &[[u8; 4]]) -> Result<(), Broke> {
    let mut bytes = Vec::new_in(global());
    encode_ids(rows, global(), &mut bytes).map_err(|e| oracle(format!("encode refused: {e}")))?;
    let view = IdView::<4>::open(&bytes).map_err(|e| oracle(format!("id round trip: open refused: {e}")))?;
    if view.len() != rows.len() {
        return Err(oracle(format!("id round trip: {} ids, not {}", view.len(), rows.len())));
    }
    for (i, row) in rows.iter().enumerate() {
        if view.get(i) != row {
            return Err(oracle(format!("id round trip: id {i}: {:?}, not {row:?}", view.get(i))));
        }
    }
    Ok(())
}

#[test]
fn id_columns_read_back_their_ids() {
    blaze_sim::check(Config::cases(300), |tape| check_ids(&draw_ids(tape)));
}

#[test]
fn the_oracle_catches_an_id_code_one_off() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || check_ids(&draw_ids(tape)));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::DictCodePlusOne, "id round trip", run);
}

#[test]
fn corrupt_bytes_are_refused_or_read_in_bounds() {
    blaze_sim::check(Config::cases(400), |tape| {
        let mut bytes = if tape.coin("from a column") {
            let mut bytes = Vec::new_in(global());
            if tape.coin("ids") {
                encode_ids(&draw_ids(tape), global(), &mut bytes).map_err(|e| oracle(format!("encode refused: {e}")))?;
            } else if tape.coin("strings") {
                let strs = draw_strs(tape);
                let refs: std::vec::Vec<&[u8]> = strs.iter().map(std::vec::Vec::as_slice).collect();
                let scheme = *tape.one_of(&StrScheme::ALL, "scheme");
                encode_strs(&refs, StrPolicy { only: Some(scheme), ..StrPolicy::RECORD }, global(), &mut bytes)
                    .map_err(|e| oracle(format!("encode refused: {e}")))?;
            } else {
                let values = draw_values(tape);
                let policy = *tape.one_of(&policies(), "policy");
                encode_ints(&values, policy, global(), &mut bytes).map_err(|e| oracle(format!("encode refused: {e}")))?;
            }
            bytes.as_slice().to_vec()
        } else {
            tape.bytes(0..=200, "bytes")
        };
        let flips = tape.int_in(0usize..=4, "flips");
        for _ in 0..flips {
            if bytes.is_empty() {
                break;
            }
            let at = tape.index(bytes.len(), "flip at");
            bytes[at] ^= tape.int_in(1u8..=255, "flip");
        }
        read_all(&bytes);
        Ok::<(), Broke>(())
    });
}

/// The layout of the smallest frame-of-reference column, byte for byte (columns.md).
#[test]
fn a_frame_of_reference_column_is_these_bytes() {
    // Not 5, 6, 7: a line through them leaves nothing to pack, so `Linear` wins with no body.
    let bytes = encode(&[5, 7, 6], IntPolicy::RECORD).unwrap_or_else(|_| panic!("encode refused"));
    // Kind 0 and `len` 3; the entry: base 5, slope 0, body at 0, width 2, scheme `For`, w2 0,
    // a zero byte; the body: 0, 2, 1 at two bits each.
    let mut want = vec![0u8, 3, 0, 0, 0];
    want.extend_from_slice(&[5, 0, 0, 0, 0, 0, 0, 0]);
    want.extend_from_slice(&[0; 8]);
    want.extend_from_slice(&[0, 0, 0, 0, 2, 1, 0, 0]);
    want.push(0b01_10_00);
    want.extend_from_slice(&[0; PAD]);
    assert_eq!(bytes.as_slice(), want.as_slice());
}
