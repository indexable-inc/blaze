use hegel::generators as gs;
use hegel::{Generator, PrintableGenerator, TestCase};

use super::*;

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The copies this module replaced, kept as the spec: a byte at a time, as every linker,
/// DWARF writer and the cold columns wrote and read them.
mod reference {
    pub fn uleb(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    pub fn sleb(out: &mut Vec<u8>, mut value: i64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
            if done {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    /// `blaze-link`'s ELF reader: refuses a 10th byte past bit 63.
    pub fn read_uleb(bytes: &[u8], at: &mut usize) -> Option<u64> {
        let (mut value, mut shift, mut i) = (0u64, 0, *at);
        loop {
            let byte = *bytes.get(i)?;
            i += 1;
            if shift == 63 && byte > 1 {
                return None;
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                *at = i;
                return Some(value);
            }
            shift += 7;
        }
    }

    pub fn read_sleb(bytes: &[u8], at: &mut usize) -> Option<i64> {
        let (mut value, mut shift, mut i) = (0i64, 0, *at);
        loop {
            let byte = *bytes.get(i)?;
            i += 1;
            if shift == 63 && byte != 0 && byte != 0x7f {
                return None;
            }
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= !0 << shift;
                }
                *at = i;
                return Some(value);
            }
        }
    }

    /// The wasm linker's reader of a `bits`-bit unsigned value.
    pub fn read_unsigned(bytes: &[u8], at: &mut usize, bits: u32) -> Option<u64> {
        let (mut value, mut shift, mut i) = (0u64, 0, *at);
        loop {
            let byte = *bytes.get(i)?;
            i += 1;
            if shift >= bits {
                return None;
            }
            let low = u64::from(byte & 0x7f);
            if bits - shift < 7 && low >> (bits - shift) != 0 {
                return None;
            }
            value |= low << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                *at = i;
                return Some(value);
            }
        }
    }

    pub fn read_signed(bytes: &[u8], at: &mut usize, bits: u32) -> Option<i64> {
        let (mut value, mut shift, mut i) = (0i64, 0, *at);
        loop {
            let byte = *bytes.get(i)?;
            i += 1;
            if shift >= bits {
                return None;
            }
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                if bits < 64 {
                    let half = 1i64 << (bits - 1);
                    if value < -half || value >= half {
                        return None;
                    }
                }
                *at = i;
                return Some(value);
            }
        }
    }

    /// The wasm linker's 5-byte padded form, and `blaze-wasm`'s padded to any length.
    pub fn padded(mut value: i64, signed: bool) -> [u8; 5] {
        let mut bytes = [0x80; 5];
        for byte in &mut bytes {
            *byte |= (value & 0x7f) as u8;
            value >>= 7;
        }
        bytes[4] &= if signed { 0x7f } else { 0x0f };
        bytes
    }

    pub fn put_padded(place: &mut [u8], value: i64) {
        let last = place.len() - 1;
        for (at, byte) in place.iter_mut().enumerate() {
            let bits = (value >> (7 * at).min(63)) as u8 & 0x7f;
            *byte = if at == last { bits } else { bits | 0x80 };
        }
    }

    pub fn skip(bytes: &[u8], at: &mut usize) -> Option<()> {
        let len = bytes.get(*at..)?.iter().take(10).position(|&byte| byte < 0x80)? + 1;
        *at += len;
        Some(())
    }
}

fn u(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    write_u64(&mut out, value);
    out
}

fn s(value: i64) -> Vec<u8> {
    let mut out = Vec::new();
    write_i64(&mut out, value);
    out
}

const UNSIGNED: [u64; 16] = [
    0,
    1,
    0x7f,
    0x80,
    300,
    0x3fff,
    0x4000,
    624_485,
    1 << 35,
    (1 << 56) - 1,
    1 << 56,
    (1 << 63) - 1,
    1 << 63,
    u32::MAX as u64,
    u64::MAX - 1,
    u64::MAX,
];

const SIGNED: [i64; 20] = [
    0,
    1,
    -1,
    63,
    64,
    -64,
    -65,
    -8,
    -123_456,
    1 << 40,
    -(1 << 40),
    (1 << 55) - 1,
    -(1 << 55),
    1 << 55,
    -(1 << 55) - 1,
    (1 << 62) - 1,
    -(1 << 62),
    i32::MIN as i64,
    i64::MIN,
    i64::MAX,
];

#[test]
fn unsigned() {
    assert_eq!(u(0), [0]);
    assert_eq!(u(127), [0x7f]);
    assert_eq!(u(128), [0x80, 0x01]);
    assert_eq!(u(300), [0xac, 0x02]);
    assert_eq!(u(624_485), [0xe5, 0x8e, 0x26]);
    assert_eq!(u(u32::MAX.into()), [0xff, 0xff, 0xff, 0xff, 0x0f]);
    assert_eq!(u(u64::MAX), [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
}

#[test]
fn signed() {
    assert_eq!(s(0), [0]);
    assert_eq!(s(-1), [0x7f]);
    assert_eq!(s(63), [0x3f]);
    assert_eq!(s(64), [0xc0, 0x00]);
    assert_eq!(s(-64), [0x40]);
    assert_eq!(s(-65), [0xbf, 0x7f]);
    assert_eq!(s(-123_456), [0xc0, 0xbb, 0x78]);
    assert_eq!(s(i32::MIN.into()), [0x80, 0x80, 0x80, 0x80, 0x78]);
    assert_eq!(s(i64::MIN), [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x7f]);
    assert_eq!(s(i64::MAX), [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
}

#[test]
fn both_signs_in_one_buffer() {
    let mut out = Vec::new();
    write_i64(&mut out, -8);
    write_i64(&mut out, 63);
    write_i64(&mut out, 64);
    write_u64(&mut out, 300);
    assert_eq!(out, [0x78, 0x3f, 0xc0, 0x00, 0xac, 0x02]);
}

#[test]
fn lengths_are_what_is_written() {
    for value in UNSIGNED {
        assert_eq!(len_u64(value), u(value).len(), "{value:#x}");
        let mut place = [0xaa; 11];
        assert_eq!(put_u64(&mut place, value), u(value).len());
        assert_eq!(&place[..u(value).len()], u(value));
        assert!(place[u(value).len()..].iter().all(|&byte| byte == 0xaa), "{value:#x}");
    }
    for value in SIGNED {
        assert_eq!(len_i64(value), s(value).len(), "{value}");
        let mut place = [0xaa; 11];
        assert_eq!(put_i64(&mut place, value), s(value).len());
        assert_eq!(&place[..s(value).len()], s(value), "{value}");
    }
}

#[test]
fn round_trips() {
    for value in UNSIGNED {
        let mut at = 0;
        assert_eq!(read_u64(&u(value), &mut at), Ok(value));
        assert_eq!(at, u(value).len());
    }
    for value in SIGNED {
        let mut at = 0;
        assert_eq!(read_i64(&s(value), &mut at), Ok(value), "{value}");
        assert_eq!(at, s(value).len());
    }
    for value in [0, 1, 127, 128, 300, 65_535, 1 << 20, u32::MAX] {
        assert_eq!(read_u32(&u(value.into()), &mut 0), Ok(value));
        assert_eq!(read_u32(&padded_u32(value), &mut 0), Ok(value));
    }
    for value in [0, 1, -1, 63, 64, -64, -65, 1 << 20, i32::MIN, i32::MAX] {
        assert_eq!(read_i32(&s(value.into()), &mut 0), Ok(value));
        assert_eq!(read_i32(&padded_i32(value), &mut 0), Ok(value));
    }
}

#[test]
fn padded() {
    assert_eq!(padded_u32(0), [0x80, 0x80, 0x80, 0x80, 0x00]);
    assert_eq!(padded_u32(5), [0x85, 0x80, 0x80, 0x80, 0x00]);
    assert_eq!(padded_u32(624_485), [0xe5, 0x8e, 0xa6, 0x80, 0x00]);
    assert_eq!(padded_u32(u32::MAX), [0xff, 0xff, 0xff, 0xff, 0x0f]);
    assert_eq!(padded_i32(0), [0x80, 0x80, 0x80, 0x80, 0x00]);
    assert_eq!(padded_i32(-1), [0xff, 0xff, 0xff, 0xff, 0x7f]);
    assert_eq!(padded_i32(i32::MIN), [0x80, 0x80, 0x80, 0x80, 0x78]);
    assert_eq!(padded_i32(i32::MAX), [0xff, 0xff, 0xff, 0xff, 0x07]);
    assert_eq!(padded_i32(0x10_0000), [0x80, 0x80, 0xc0, 0x80, 0x00]);
    let mut place = [0; 3];
    assert_eq!(put_padded_u64(&mut place, 5), Ok(()));
    assert_eq!(place, [0x85, 0x80, 0x00]);
    assert_eq!(put_padded_u64(&mut place, 1 << 21), Err(LebError::Overflow));
    assert_eq!(place, [0x85, 0x80, 0x00], "nothing is written for a value that doesn't fit");
    assert_eq!(put_padded_i64(&mut place, -1), Ok(()));
    assert_eq!(place, [0xff, 0xff, 0x7f]);
    assert_eq!(put_padded_i64(&mut place, 1 << 20), Err(LebError::Overflow));
    assert_eq!(put_padded_i64(&mut [], 0), Err(LebError::Overflow));
    assert_eq!(put_padded_u64(&mut [0; 11], 0), Err(LebError::Overflow));
    let mut ten = [0; 10];
    assert_eq!(put_padded_u64(&mut ten, u64::MAX), Ok(()));
    assert_eq!(ten.to_vec(), u(u64::MAX));
    assert_eq!(put_padded_i64(&mut ten, i64::MIN), Ok(()));
    assert_eq!(ten.to_vec(), s(i64::MIN));
}

#[test]
fn refuses_what_does_not_fit() {
    let over = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
    assert_eq!(read_u64(&over, &mut 0), Err(LebError::Overflow));
    let long = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
    assert_eq!(read_u64(&long, &mut 0), Err(LebError::Overflow));
    assert_eq!(read_i64(&long, &mut 0), Err(LebError::Overflow));
    assert_eq!(skip(&long, &mut 0), Err(LebError::Overflow));
    let signed_over = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
    assert_eq!(read_i64(&signed_over, &mut 0), Err(LebError::Overflow));
    assert_eq!(read_u32(&[0xff, 0xff, 0xff, 0xff, 0x1f], &mut 0), Err(LebError::Overflow));
    assert_eq!(read_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00], &mut 0), Err(LebError::Overflow));
    assert_eq!(read_i32(&[0xff, 0xff, 0xff, 0xff, 0x0f], &mut 0), Err(LebError::Overflow));
    assert_eq!(read_i32(&[0x80, 0x80, 0x80, 0x80, 0x70], &mut 0), Err(LebError::Overflow));
    let cuts: [&[u8]; 5] = [&[], &[0x80], &[0xff; 7], &[0xff; 8], &[0xff; 9]];
    for cut in cuts {
        let mut at = 0;
        assert_eq!(read_u64(cut, &mut at), Err(LebError::Truncated), "{cut:?}");
        assert_eq!(read_i64(cut, &mut at), Err(LebError::Truncated), "{cut:?}");
        assert_eq!(skip(cut, &mut at), Err(LebError::Truncated), "{cut:?}");
        assert_eq!(at, 0, "an error leaves the position");
    }
    assert_eq!(read_u64(&[0], &mut 2), Err(LebError::Truncated));
}

#[test]
fn reads_from_a_position() {
    let mut bytes = vec![0xaa];
    write_u64(&mut bytes, 624_485);
    write_i64(&mut bytes, -123_456);
    write_u64(&mut bytes, u64::MAX);
    bytes.push(7);
    let mut at = 1;
    assert_eq!(read_u64(&bytes, &mut at), Ok(624_485));
    assert_eq!(read_i64(&bytes, &mut at), Ok(-123_456));
    let before = at;
    assert_eq!(skip(&bytes, &mut at), Ok(()));
    assert_eq!(at - before, 10);
    assert_eq!(read_u64(&bytes, &mut at), Ok(7));
    assert_eq!(at, bytes.len());
}

#[test]
fn reads_and_writes_into_room_allocate_nothing() {
    let mut out = Vec::with_capacity(64);
    let mut place = [0; 10];
    let bytes = [0xe5, 0x8e, 0x26, 0xc0, 0xbb, 0x78, 0x80, 0x80, 0x80, 0x80, 0x00, 0, 0];
    let read = blaze_alloc_count::assert_at_most(0, "LEB128 reads and writes into room", || {
        write_u64(&mut out, 624_485);
        write_i64(&mut out, -123_456);
        write_u64(&mut out, u64::MAX);
        let mut at = 0;
        let read = (
            read_u64(&bytes, &mut at),
            read_i64(&bytes, &mut at),
            read_u32(&bytes, &mut at),
            skip(&bytes, &mut at),
        );
        (read, padded_u32(5), padded_i32(-1), put_u64(&mut place, u64::MAX))
    });
    assert_eq!(read, ((Ok(624_485), Ok(-123_456), Ok(0), Ok(())), padded_u32(5), padded_i32(-1), 10));
    assert_eq!(out[..6], [0xe5, 0x8e, 0x26, 0xc0, 0xbb, 0x78]);
}

#[test]
fn an_exactly_sized_vector_never_grows() {
    for value in UNSIGNED {
        let mut out = Vec::with_capacity(len_u64(value));
        let capacity = out.capacity();
        write_u64(&mut out, value);
        assert_eq!((out.capacity(), out.len()), (capacity, len_u64(value)), "{value:#x}");
    }
    for value in SIGNED {
        let mut out = Vec::with_capacity(len_i64(value));
        let capacity = out.capacity();
        write_i64(&mut out, value);
        assert_eq!((out.capacity(), out.len()), (capacity, len_i64(value)), "{value}");
    }
}

/// A value of every length: `bits` random bits, so short encodings are drawn as often as long.
fn sized() -> impl PrintableGenerator<u64> {
    gs::tuples!(gs::integers::<u32>().max_value(64), gs::integers::<u64>()).map(|(bits, value)| match bits {
        64 => value,
        bits => value & ((1 << bits) - 1),
    })
}

#[hegel::test]
fn writes_match_the_reference(tc: TestCase) {
    let value = tc.draw(sized());
    let sign = tc.draw(gs::booleans());
    let lead: usize = tc.draw(gs::integers().max_value(11));
    let signed = if sign { !value as i64 } else { value as i64 };
    let mut ours = vec![0xaa; lead];
    let mut theirs = vec![0xaa; lead];
    write_u64(&mut ours, value);
    reference::uleb(&mut theirs, value);
    assert_eq!(&ours, &theirs);
    assert_eq!(len_u64(value), ours.len() - lead);
    write_i64(&mut ours, signed);
    reference::sleb(&mut theirs, signed);
    assert_eq!(&ours, &theirs);
    assert_eq!(len_i64(signed), ours.len() - lead - len_u64(value));
    let mut place = [0; MAX_LEN];
    let len = put_i64(&mut place, signed);
    assert_eq!(&place[..len], &theirs[lead + len_u64(value)..]);
}

#[hegel::test]
fn reads_match_the_reference(tc: TestCase) {
    let bytes: Vec<u8> = tc.draw(gs::binary().max_size(23));
    let at: usize = tc.draw(gs::integers().max_value(3));
    let reads = |read: &dyn Fn(&mut usize) -> Option<i128>| {
        let mut at = at;
        (read(&mut at), at)
    };
    let ours = reads(&|at| read_u64(&bytes, at).ok().map(i128::from));
    let theirs = reads(&|at| reference::read_uleb(&bytes, at).map(i128::from));
    assert_eq!(ours, theirs);
    let ours = reads(&|at| read_i64(&bytes, at).ok().map(i128::from));
    let theirs = reads(&|at| reference::read_sleb(&bytes, at).map(i128::from));
    assert_eq!(ours, theirs);
    let ours = reads(&|at| read_u32(&bytes, at).ok().map(i128::from));
    let theirs = reads(&|at| reference::read_unsigned(&bytes, at, 32).map(i128::from));
    assert_eq!(ours, theirs);
    let ours = reads(&|at| read_i32(&bytes, at).ok().map(i128::from));
    let theirs = reads(&|at| reference::read_signed(&bytes, at, 32).map(i128::from));
    assert_eq!(ours, theirs);
    let ours = reads(&|at| skip(&bytes, at).ok().map(|()| 0));
    let theirs = reads(&|at| reference::skip(&bytes, at).map(|()| 0));
    assert_eq!(ours, theirs);
}

#[hegel::test]
fn written_values_read_back(tc: TestCase) {
    let value = tc.draw(sized());
    let sign = tc.draw(gs::booleans());
    let tail: Vec<u8> = tc.draw(gs::binary().max_size(9));
    let signed = if sign { !value as i64 } else { value as i64 };
    let mut bytes = Vec::new();
    write_u64(&mut bytes, value);
    write_i64(&mut bytes, signed);
    bytes.extend_from_slice(&tail);
    let mut at = 0;
    assert_eq!(read_u64(&bytes, &mut at), Ok(value));
    assert_eq!(read_i64(&bytes, &mut at), Ok(signed));
    assert_eq!(at, bytes.len() - tail.len());
}

#[hegel::test]
fn padded_matches_the_reference(tc: TestCase) {
    let value: i32 = tc.draw(gs::integers());
    let len: usize = tc.draw(gs::integers().min_value(1).max_value(MAX_LEN));
    let wide: i64 = tc.draw(gs::integers());
    assert_eq!(padded_u32(value as u32), reference::padded(i64::from(value as u32), false));
    assert_eq!(padded_i32(value), reference::padded(value.into(), true));
    let (mut ours, mut theirs) = ([0; MAX_LEN], [0; MAX_LEN]);
    let fits = len_i64(wide) <= len;
    assert_eq!(put_padded_i64(&mut ours[..len], wide).is_ok(), fits);
    if fits {
        reference::put_padded(&mut theirs[..len], wide);
        assert_eq!(ours, theirs);
        assert_eq!(read_i64(&ours[..len], &mut 0), Ok(wide));
    }
    let wide = wide as u64;
    let fits = len_u64(wide) <= len;
    assert_eq!(put_padded_u64(&mut ours[..len], wide).is_ok(), fits);
    if fits {
        assert_eq!(read_u64(&ours[..len], &mut 0), Ok(wide));
    }
}
