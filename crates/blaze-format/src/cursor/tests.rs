use hegel::TestCase;
use hegel::generators as gs;

use super::*;

/// The copies this module replaced, kept as the spec: `blaze-history`'s `Bytes` (fixed-width
/// fields) and `blaze-usercode`'s test-wire `Cursor` (with `u32`-length-prefixed bytes and
/// text), as they were.
mod reference {
    pub struct Bytes<'a> {
        pub bytes: &'a [u8],
        pub at: usize,
    }

    impl<'a> Bytes<'a> {
        pub fn take(&mut self, len: usize) -> Option<&'a [u8]> {
            let it = self.at.checked_add(len).and_then(|end| self.bytes.get(self.at..end))?;
            self.at += len;
            Some(it)
        }

        pub fn u64(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
        }

        pub fn u32(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
        }
    }

    pub struct Cursor<'a> {
        pub bytes: &'a [u8],
        pub at: usize,
    }

    impl<'a> Cursor<'a> {
        pub fn take(&mut self, len: usize) -> Option<&'a [u8]> {
            if self.bytes.len() - self.at < len {
                return None;
            }
            let taken = &self.bytes[self.at..self.at + len];
            self.at += len;
            Some(taken)
        }

        pub fn u8(&mut self) -> Option<u8> {
            Some(self.take(1)?[0])
        }

        pub fn u16(&mut self) -> Option<u16> {
            let b = self.take(2)?;
            Some(u16::from_le_bytes([b[0], b[1]]))
        }

        pub fn bytes(&mut self) -> Option<&'a [u8]> {
            let len = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
            self.take(len)
        }

        pub fn string(&mut self) -> Option<&'a str> {
            std::str::from_utf8(self.bytes()?).ok()
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Take(usize),
    U8,
    U16,
    U32,
    U64,
    Bytes,
    Str,
}

#[derive(PartialEq, Eq, Debug)]
enum Value<'a> {
    Slice(&'a [u8]),
    Int(u64),
    Text(&'a str),
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    match tc.draw(gs::integers::<u8>().max_value(6)) {
        0 => Op::Take(tc.draw(gs::integers().max_value(11))),
        1 => Op::U8,
        2 => Op::U16,
        3 => Op::U32,
        4 => Op::U64,
        5 => Op::Bytes,
        _ => Op::Str,
    }
}

/// `op` by the copy that had it, from `*at`, moving `*at` as that copy did.
fn by_reference<'a>(bytes: &'a [u8], at: &mut usize, op: Op) -> Option<Value<'a>> {
    let mut history = reference::Bytes { bytes, at: *at };
    let mut wire = reference::Cursor { bytes, at: *at };
    let (value, moved) = match op {
        Op::Take(len) => (history.take(len).map(Value::Slice), history.at),
        Op::U32 => (history.u32().map(|it| Value::Int(it.into())), history.at),
        Op::U64 => (history.u64().map(Value::Int), history.at),
        Op::U8 => (wire.u8().map(|it| Value::Int(it.into())), wire.at),
        Op::U16 => (wire.u16().map(|it| Value::Int(it.into())), wire.at),
        Op::Bytes => (wire.bytes().map(Value::Slice), wire.at),
        Op::Str => (wire.string().map(Value::Text), wire.at),
    };
    *at = moved;
    value
}

type Leb<T> = fn(&[u8], &mut usize) -> Result<T, LebError>;

/// [`Reader`]'s LEB128 read against `leb`'s, from the start of `bytes`.
fn same_leb<T: PartialEq + std::fmt::Debug>(
    bytes: &[u8],
    theirs: Leb<T>,
    ours: impl Fn(&mut Reader<'_>) -> Result<T, Error>,
) {
    let mut at = 0;
    let mut reader = Reader::new(bytes);
    match (theirs(bytes, &mut at), ours(&mut reader)) {
        (Ok(theirs), Ok(ours)) => {
            assert_eq!(theirs, ours);
            assert_eq!(reader.at(), at);
        }
        (Err(theirs), Err(ours)) => {
            assert_eq!(ours, Error { at: 0, wanted: Wanted::Leb(theirs) });
            assert_eq!(reader.at(), 0);
        }
        (theirs, ours) => panic!("leb {:?}, reader {:?}", theirs, ours),
    }
}

fn by_reader<'a>(reader: &mut Reader<'a>, op: Op) -> Result<Value<'a>, Error> {
    Ok(match op {
        Op::Take(len) => Value::Slice(reader.take(len)?),
        Op::U8 => Value::Int(reader.u8()?.into()),
        Op::U16 => Value::Int(reader.u16()?.into()),
        Op::U32 => Value::Int(reader.u32()?.into()),
        Op::U64 => Value::Int(reader.u64()?),
        Op::Bytes => Value::Slice(reader.bytes_u32()?),
        Op::Str => Value::Text(reader.str_u32()?),
    })
}

#[hegel::test]
fn reads_what_the_copies_read(tc: TestCase) {
    let bytes: Vec<u8> = tc.draw(gs::binary().max_size(63));
    let ops = tc.draw(gs::vecs(op()).max_size(23));
    let mut at = 0;
    let mut reader = Reader::new(&bytes);
    for op in ops {
        let before = reader.at();
        let ours = by_reader(&mut reader, op);
        let theirs = by_reference(&bytes, &mut at, op);
        match (ours, theirs) {
            (Ok(ours), Some(theirs)) => {
                assert_eq!(ours, theirs);
                assert_eq!(reader.at(), at);
            }
            (Err(error), None) => {
                assert_eq!(reader.at(), before, "a failed read moves nothing");
                assert!(error.at >= before && error.at <= bytes.len());
                break;
            }
            (ours, theirs) => panic!("{:?}: ours {:?}, theirs {:?}", op, ours, theirs),
        }
    }
}

#[hegel::test]
fn reads_back_what_put_wrote(tc: TestCase) {
    let (a, b, c, d, e): (u8, u16, u32, u64, u128) = tc.draw(gs::tuples!(
        gs::integers(),
        gs::integers(),
        gs::integers(),
        gs::integers(),
        gs::integers(),
    ));
    let (f, g, h, i): (i8, i16, i32, i64) =
        tc.draw(gs::tuples!(gs::integers(), gs::integers(), gs::integers(), gs::integers()));
    let (uleb, sleb): (u64, i64) = tc.draw(gs::tuples!(gs::integers(), gs::integers()));
    let raw: Vec<u8> = tc.draw(gs::binary().max_size(19));
    let text = tc.draw(gs::text().alphabet("abcdefghijklmnopqrstuvwxyzé").max_size(12));
    let cstr = tc.draw(gs::vecs(gs::integers::<u8>().min_value(1)).max_size(11));
    let mut out = Vec::new();
    out.put_u8(a);
    out.put_u16(b);
    out.put_u32(c);
    out.put_u64(d);
    out.put_u128(e);
    out.put_i8(f);
    out.put_i16(g);
    out.put_i32(h);
    out.put_i64(i);
    out.put_uleb(uleb);
    out.put_sleb(sleb);
    out.put_bytes_u32(&raw);
    out.put_bytes_u64(&raw);
    out.put_bytes_uleb(&raw);
    out.put_bytes_u32(text.as_bytes());
    out.put_bytes_u64(text.as_bytes());
    out.put_bytes_uleb(text.as_bytes());
    out.put_cstr(&cstr);
    out.put_bytes(&raw);

    let mut read = Reader::new(&out);
    assert_eq!(read.u8().unwrap(), a);
    assert_eq!(read.u16().unwrap(), b);
    assert_eq!(read.u32().unwrap(), c);
    assert_eq!(read.u64().unwrap(), d);
    assert_eq!(read.u128().unwrap(), e);
    assert_eq!(read.i8().unwrap(), f);
    assert_eq!(read.i16().unwrap(), g);
    assert_eq!(read.i32().unwrap(), h);
    assert_eq!(read.i64().unwrap(), i);
    assert_eq!(read.uleb().unwrap(), uleb);
    assert_eq!(read.sleb().unwrap(), sleb);
    assert_eq!(read.bytes_u32().unwrap(), &raw[..]);
    assert_eq!(read.bytes_u64().unwrap(), &raw[..]);
    assert_eq!(read.bytes_uleb().unwrap(), &raw[..]);
    assert_eq!(read.str_u32().unwrap(), &text[..]);
    assert_eq!(read.str_u64().unwrap(), &text[..]);
    assert_eq!(read.str_uleb().unwrap(), &text[..]);
    assert_eq!(read.cstr().unwrap(), &cstr[..]);
    assert_eq!(read.take(raw.len()).unwrap(), &raw[..]);
    assert!(read.is_done());
    assert_eq!(read.finish(), Ok(()));
}

#[hegel::test]
fn lebs_read_as_leb_reads_them(tc: TestCase) {
    let bytes: Vec<u8> = tc.draw(gs::binary().max_size(15));
    same_leb(&bytes, leb::read_u64, |read: &mut Reader<'_>| read.uleb());
    same_leb(&bytes, leb::read_i64, |read: &mut Reader<'_>| read.sleb());
    same_leb(&bytes, leb::read_u32, |read: &mut Reader<'_>| read.uleb32());
    same_leb(&bytes, leb::read_i32, |read: &mut Reader<'_>| read.sleb32());
    same_leb(&bytes, leb::skip, |read: &mut Reader<'_>| read.skip_leb());
}

#[test]
fn integers_are_little_endian() {
    let bytes: Vec<u8> = (1..=16).collect();
    let mut read = Reader::new(&bytes);
    assert_eq!(read.u8(), Ok(0x01));
    assert_eq!(read.u16(), Ok(0x0302));
    assert_eq!(read.u32(), Ok(0x0706_0504));
    assert_eq!(read.u64(), Ok(0x0f0e_0d0c_0b0a_0908));
    assert_eq!(read.at(), 15);
    assert_eq!(read.left(), 1);
    assert_eq!(read.remaining(), &[16]);
    let mut read = Reader::new(&bytes);
    assert_eq!(read.u128(), Ok(u128::from_le_bytes(bytes[..].try_into().unwrap())));
    let mut read = Reader::new(&[0xff, 0xfe, 0xff, 0xfd, 0xff, 0xff, 0xff]);
    assert_eq!(read.i8(), Ok(-1));
    assert_eq!(read.i16(), Ok(-2));
    assert_eq!(read.i32(), Ok(-3));
}

#[test]
fn a_short_read_names_where_and_what_and_moves_nothing() {
    let mut read = Reader::new(&[1, 2, 3, 4, 5, 6]);
    read.skip(3).unwrap();
    assert_eq!(read.u32(), Err(Error { at: 3, wanted: Wanted::Bytes { len: 4, left: 3 } }));
    assert_eq!(read.take(7), Err(Error { at: 3, wanted: Wanted::Bytes { len: 7, left: 3 } }));
    assert_eq!(read.take(usize::MAX), Err(Error { at: 3, wanted: Wanted::Bytes { len: usize::MAX, left: 3 } }));
    assert_eq!(read.at(), 3);
    assert_eq!(read.array::<3>(), Ok(&[4, 5, 6]));
    assert_eq!(read.u8(), Err(Error { at: 6, wanted: Wanted::Bytes { len: 1, left: 0 } }));
    assert_eq!(
        read.u64().unwrap_err().to_string(),
        "at byte 6: 8 bytes wanted, 0 left"
    );
    let message: String = read.u16().unwrap_err().into();
    assert_eq!(message, "at byte 6: 2 bytes wanted, 0 left");
    assert_eq!(read.u16().unwrap_err().as_str(), "bytes that end early");
}

#[test]
fn a_prefix_past_the_end_is_refused_and_unread() {
    let mut out = Vec::new();
    out.put_u32(10);
    out.put_bytes(b"abc");
    let mut read = Reader::new(&out);
    assert_eq!(read.bytes_u32(), Err(Error { at: 4, wanted: Wanted::Bytes { len: 10, left: 3 } }));
    assert_eq!(read.at(), 0, "back before the length");
    let mut out = Vec::new();
    out.put_u64(u64::MAX);
    let mut read = Reader::new(&out);
    assert!(matches!(read.bytes_u64(), Err(Error { at: 8, wanted: Wanted::Bytes { left: 0, .. } })));
    assert_eq!(read.at(), 0);
    let mut out = Vec::new();
    out.put_bytes_u32(&[0xff, 0xfe]);
    let mut read = Reader::new(&out);
    assert_eq!(read.str_u32(), Err(Error { at: 6, wanted: Wanted::Utf8 { len: 2 } }));
    assert_eq!(read.at(), 0);
    assert_eq!(read.bytes_u32(), Ok(&[0xff, 0xfe][..]));
}

#[test]
fn a_count_is_checked_against_the_bytes_left() {
    let mut out = Vec::new();
    out.put_u32(3);
    out.put_bytes(&[0; 12]);
    assert_eq!(Reader::new(&out).count_u32(4), Ok(3));
    assert_eq!(Reader::new(&out).count_u32(0), Ok(3), "each is at least a byte");
    let mut read = Reader::new(&out);
    assert_eq!(read.count_u32(5), Err(Error { at: 4, wanted: Wanted::Count { count: 3, left: 12 } }));
    assert_eq!(read.at(), 0);
    let mut out = Vec::new();
    out.put_u64(u64::MAX / 2);
    out.put_bytes(&[0; 8]);
    assert!(matches!(Reader::new(&out).count_u64(4), Err(Error { wanted: Wanted::Count { .. }, .. })));
    let mut out = Vec::new();
    out.put_uleb(2);
    out.put_bytes(&[7, 7]);
    let mut read = Reader::new(&out);
    assert_eq!(read.count_uleb(1), Ok(2));
    assert_eq!(read.take(2), Ok(&[7, 7][..]));
    assert_eq!(read.finish(), Ok(()));
}

#[test]
fn c_strings_end_at_their_nul() {
    let mut read = Reader::new(b"zR\0\0abc");
    assert_eq!(read.cstr(), Ok(&b"zR"[..]));
    assert_eq!(read.cstr(), Ok(&b""[..]));
    assert_eq!(read.at(), 4);
    assert_eq!(read.cstr(), Err(Error { at: 4, wanted: Wanted::Nul { left: 3 } }));
    assert_eq!(read.at(), 4);
}

#[test]
fn seeking_and_the_end() {
    let bytes = [1, 2, 3, 4];
    let mut read = Reader::starting_at(&bytes, 2).unwrap();
    assert_eq!(read.u8(), Ok(3));
    assert_eq!(read.finish(), Err(Error { at: 3, wanted: Wanted::End { left: 1 } }));
    assert_eq!(read.finish().unwrap_err().to_string(), "at byte 3: 1 bytes past the end");
    read.seek(0).unwrap();
    assert_eq!(read.u16(), Ok(0x0201));
    assert_eq!(read.bytes(), &bytes);
    assert_eq!(read.take_rest(), &[3, 4]);
    assert!(read.is_done());
    assert_eq!(read.at(), 4);
    read.seek(4).unwrap();
    assert_eq!(read.seek(5), Err(Error { at: 4, wanted: Wanted::Bytes { len: 1, left: 0 } }));
    assert!(Reader::starting_at(&bytes, 9).is_err());
}

#[test]
fn a_bad_leb_names_its_byte() {
    let mut read = Reader::new(&[0, 0x80, 0x80]);
    read.u8().unwrap();
    assert_eq!(read.uleb(), Err(Error { at: 1, wanted: Wanted::Leb(LebError::Truncated) }));
    assert_eq!(read.at(), 1);
    let mut read = Reader::new(&[0xff; 11]);
    assert_eq!(read.uleb(), Err(Error { at: 0, wanted: Wanted::Leb(LebError::Overflow) }));
    assert_eq!(read.uleb().unwrap_err().as_str(), LebError::Overflow.as_str());
}

/// `blaze-link`'s Mach-O dylib reader's `uleb_limits`: the largest value reads back, and a 10th
/// byte past bit 63 is refused.
#[test]
fn uleb_limits() {
    let mut max = Vec::new();
    max.put_uleb(u64::MAX);
    let mut read = Reader::new(&max);
    assert_eq!(read.uleb(), Ok(u64::MAX));
    assert!(read.is_done());
    let over = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
    assert_eq!(Reader::new(&over).uleb(), Err(Error { at: 0, wanted: Wanted::Leb(LebError::Overflow) }));
}

#[test]
fn patches_write_over_what_was_written() {
    let mut out = Vec::new();
    out.put_u32(0);
    out.put_u64(0);
    out.patch_u32(0, 0xdead_beef);
    out.patch_u64(4, 7);
    let mut read = Reader::new(&out);
    assert_eq!(read.u32(), Ok(0xdead_beef));
    assert_eq!(read.u64(), Ok(7));
}

/// `blaze-link`'s three `align`s (Mach-O, ELF, PE writers), which this replaced.
#[test]
fn padding_reaches_the_next_multiple() {
    let mut out = vec![1u8; 5];
    out.pad_to(8);
    assert_eq!(out, [1, 1, 1, 1, 1, 0, 0, 0]);
    out.pad_to(8);
    assert_eq!(out.len(), 8, "already a multiple: nothing added");
    out.pad_to(1);
    assert_eq!(out.len(), 8);
    let mut empty = Vec::<u8>::new();
    empty.pad_to(16);
    assert!(empty.is_empty());
}

#[test]
#[should_panic(expected = "a C string holding a NUL")]
fn a_c_string_with_a_nul_is_refused() {
    Vec::<u8>::new().put_cstr(b"a\0b");
}

#[test]
fn reads_and_writes_into_room_allocate_nothing() {
    let mut out = Vec::with_capacity(256);
    let bytes = blaze_alloc_count::assert_at_most(0, "cursor writes into room", || {
        out.put_u8(1);
        out.put_u16(2);
        out.put_u32(3);
        out.put_u64(4);
        out.put_uleb(300);
        out.put_sleb(-300);
        out.put_bytes_u32(b"name");
        out.put_cstr(b"zR");
        out.patch_u32(1, 9);
        out.len()
    });
    assert_eq!(bytes, out.len());
    blaze_alloc_count::assert_at_most(0, "cursor reads", || {
        let mut read = Reader::new(&out);
        assert_eq!(read.u8(), Ok(1));
        read.u16().unwrap();
        read.u32().unwrap();
        assert_eq!(read.u64(), Ok(4));
        assert_eq!(read.uleb(), Ok(300));
        assert_eq!(read.sleb(), Ok(-300));
        assert_eq!(read.str_u32(), Ok("name"));
        assert_eq!(read.cstr(), Ok(&b"zR"[..]));
        assert!(read.u64().is_err());
        assert!(read.finish().is_ok());
    });
}
