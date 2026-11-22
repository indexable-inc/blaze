//! The delta column and LEB128 against their definitions, and `Dict` against a list of first
//! sights.

use std::{format, vec, vec::Vec as StdVec};

use blaze_own::Vec;
use blaze_sim::{Broke, Config, Tape};

use super::{Failing, Why, counted, ensure, no_leaks, planted, small, step, under_every_failure};
use crate::{
    Dict, Id,
    codec::{CodecError, delta_decode, delta_encode, read_uleb128, uleb128_len, unzigzag, write_uleb128, zigzag},
    plant::Plant,
};

/// A value of any scale and sign: small differences often, the extremes sometimes.
fn value(tape: &Tape, last: i64) -> i64 {
    match tape.below(4, "kind") {
        0 => tape.next_u64("any").cast_signed(),
        1 => [i64::MIN, i64::MAX, 0, -1][tape.index(4, "edge")],
        _ => last.wrapping_add(tape.below(200, "step").cast_signed() - 100),
    }
}

fn delta_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let mut last = 0;
        let values = tape.vec(0..=40, "values", |tape| {
            last = value(tape, last);
            last
        });
        let mut bytes: Vec<u8, _> = Vec::new_in(&alloc);
        bytes.push(0xEE);
        delta_encode(&values, &mut bytes).map_err(|e| Broke::Harness(format!("{e}")))?;
        let mut back = vec![0i64; values.len()];
        let read = delta_decode(&bytes[1..], &mut back);
        ensure(read == Ok(bytes.len() - 1), || format!("the round trip read {read:?} of {} bytes", bytes.len() - 1))?;
        ensure(back == values, || format!("the round trip gave {back:?} for {values:?}"))?;
        if !values.is_empty() {
            let cut = &bytes[1..bytes.len() - 1];
            ensure(matches!(delta_decode(cut, &mut back), Err(CodecError::Truncated { .. })), || {
                format!("a round trip cut short decoded")
            })?;
        }
        for &v in &values {
            let z = zigzag(v);
            ensure(unzigzag(z) == v, || format!("zigzag of {v} does not come back"))?;
            let mut one: Vec<u8, _> = Vec::new_in(&alloc);
            write_uleb128(z, &mut one).map_err(|e| Broke::Harness(format!("{e}")))?;
            ensure(one.len() == uleb128_len(z), || format!("uleb128_len of {z} is not the round trip's {}", one.len()))?;
            let mut at = 0;
            ensure(read_uleb128(&one, &mut at, 0) == Ok(z) && at == one.len(), || format!("the round trip of {z} differs"))?;
        }
    }
    no_leaks(&alloc)
}

/// The refinement: delta columns, LEB128 codecs, and `Dict` match their specifications and reference lists without dropping bytes or reusing codes.
#[test]
fn delta_column_round_trips() {
    blaze_sim::check(Config::cases(400), delta_case);
}

#[test]
fn the_oracle_catches_a_dropped_last_byte() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::LebDropsLastByte, "round trip", |tape, p| {
        planted(p, tape, delta_case)
    });
}

#[test]
fn leb128_is_the_spec_and_refuses_overlong_values() {
    let mut at = 0;
    assert_eq!(read_uleb128(&[0xE5, 0x8E, 0x26], &mut at, 0), Ok(624_485));
    assert_eq!(at, 3);
    assert_eq!([zigzag(0), zigzag(-1), zigzag(1), zigzag(-2), zigzag(i64::MIN)], [0, 1, 2, 3, u64::MAX]);
    let max = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
    let mut at = 0;
    assert_eq!(read_uleb128(&max, &mut at, 0), Ok(u64::MAX));
    let over = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02];
    assert_eq!(read_uleb128(&over, &mut 0, 0), Err(CodecError::Overlong { at: 0 }));
    let long = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
    assert_eq!(read_uleb128(&long, &mut 0, 0), Err(CodecError::Overlong { at: 0 }));
}

id! {
    /// A dictionary code under test.
    struct Code;
}

fn dict_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let mut dict: Dict<Code, u32, _> = Dict::new_in(&alloc);
        let mut seen: StdVec<u32> = StdVec::new();
        let distinct = 1 + small(tape, 200, "distinct");
        for _ in 0..tape.below(300, "rows") {
            let v = small(tape, distinct, "value");
            let code = dict.push(v);
            let want = match seen.iter().position(|&s| s == v) {
                Some(at) => at,
                None => {
                    seen.push(v);
                    seen.len() - 1
                }
            };
            ensure(code.index() == want, || format!("the code of {v} is {code:?}, its first sight {want}"))?;
            ensure(*dict.value(code) == v, || format!("the value of code {code:?} is not {v}"))?;
        }
        ensure(dict.values() == seen.as_slice(), || format!("the values differ from the first sights"))?;
        for (at, &v) in seen.iter().enumerate() {
            ensure(dict.code_of(&v).map(Code::index) == Some(at), || format!("code_of {v} said wrong"))?;
        }
        ensure(dict.code_of(&distinct).is_none(), || format!("code_of a value never pushed is a code"))?;
    }
    no_leaks(&alloc)
}

#[test]
fn dict_codes_values_by_first_sight() {
    blaze_sim::check(Config::cases(300), dict_case);
}

#[test]
fn the_oracle_catches_a_reused_code() {
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::DictReusesCode, "first sight", |tape, p| {
        planted(p, tape, dict_case)
    });
}

#[test]
fn codec_and_dict_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("delta_encode", |alloc| {
        let mut bytes: Vec<u8, _> = Vec::new_in(alloc);
        let look = |b: &Vec<u8, &Failing>| StdVec::from(b.as_slice());
        let to_why = |e: CodecError| match e {
            CodecError::Alloc(failure) => Why::Refused(failure),
            other => Why::Changed(format!("{other}")),
        };
        step(&mut bytes, look, |b| write_uleb128(300, b))?;
        step(&mut bytes, look, |b| delta_encode(&[1, 2, 3, -100, i64::MAX], b).map_err(to_why))?;
        step(&mut bytes, look, |b| delta_encode(&[7; 200], b).map_err(to_why))?;
        Ok(())
    })?;
    under_every_failure("Dict", |alloc| {
        let mut dict: Dict<Code, u64, _> = Dict::new_in(alloc);
        let look = |d: &Dict<Code, u64, &Failing>| (d.codes().iter().map(|c| c.to_u32()).collect::<StdVec<_>>(), StdVec::from(d.values()));
        for n in [1u64, 2, 1, 3, 4, 5, 6, 7, 8, 9, 2, 10, 11, 12] {
            step(&mut dict, look, |d| {
                d.try_push(n).map_err(|r| if r.value == n { Why::from(r) } else { Why::Changed(format!("{} came back", r.value)) })
            })?;
        }
        Ok(())
    })?;
    Ok(())
}
