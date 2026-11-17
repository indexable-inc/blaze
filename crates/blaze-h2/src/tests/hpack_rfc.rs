//! RFC 7541's own examples (Appendix C), byte for byte: each block as the RFC prints it, the
//! fields it decodes to, and the dynamic table after it (entries newest first, and its size),
//! for our encoder's table and our decoder's both. The raw-string examples are written as the
//! representation bytes the RFC prints with the literal text between them (the same bytes, with
//! no hand-typed hex for the ASCII); the Huffman ones are the RFC's hex.
//!
//! Each test prints cargo's `test tests::hpack_rfc::<name> ... ok`.

use std::vec::Vec as StdVec;

use blaze_own::AVec;

use super::{alloc, bytes, cat, hex, show};
use crate::{
    head::{Field, Fields},
    hpack::{
        Decoder, Encoder, HpackFault, Strings, get_int, huffman,
        huffman::{CANON, EOS, LENS},
        put_int,
        table::{STATIC, Table},
    },
};

type Pairs = StdVec<(StdVec<u8>, StdVec<u8>)>;

fn pairs(list: &[(&str, &str)]) -> Pairs {
    list.iter().map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec())).collect()
}

fn table_pairs(table: &Table<'_>) -> Pairs {
    table.iter().map(|(n, v)| (n.to_vec(), v.to_vec())).collect()
}

/// One block of an Appendix C sequence.
struct Step<'s> {
    fields: &'s [(&'s str, &'s str)],
    block: StdVec<u8>,
    /// The dynamic table after it, newest first.
    table: &'s [(&'s str, &'s str)],
    size: u64,
}

/// Runs a sequence with one encoder and one decoder that both start at `max` bytes.
fn sequence(name: &str, strings: Strings, max: u32, steps: &[Step<'_>]) {
    let mut encoder = Encoder::agreed(max, strings, alloc());
    let mut decoder = Decoder::new(max, alloc());
    for (k, step) in steps.iter().enumerate() {
        let fields: StdVec<Field<'_>> =
            step.fields.iter().map(|(n, v)| Field::new(n.as_bytes(), v.as_bytes())).collect();
        let mut out = AVec::new_in(alloc());
        encoder.encode(fields.iter().copied(), &mut out).expect("encode");
        assert_eq!(bytes(&out), step.block, "{name} block {}: the encoder's bytes differ from the RFC's", k + 1);
        let mut got = Fields::new_in(alloc());
        decoder.decode(&step.block, &mut got, u32::MAX).expect("decode");
        let got: Pairs = got.iter().map(|f| (f.name.to_vec(), f.value.to_vec())).collect();
        assert_eq!(got, pairs(step.fields), "{name} block {}: decoded\n{}", k + 1, show(&got));
        assert_eq!(table_pairs(encoder.table()), pairs(step.table), "{name} block {}: encoder's table", k + 1);
        assert_eq!(table_pairs(decoder.table()), pairs(step.table), "{name} block {}: decoder's table", k + 1);
        assert_eq!(encoder.table().size(), step.size, "{name} block {}: encoder's table size", k + 1);
        assert_eq!(decoder.table().size(), step.size, "{name} block {}: decoder's table size", k + 1);
    }
}

/// C.1.1 to C.1.3: 10 with a 5-bit prefix, 1337 with a 5-bit prefix, 42 at an octet boundary.
#[test]
fn c1_integers() {
    let cases: [(u32, u8, &[u8]); 3] = [(10, 5, &[0x0a]), (1337, 5, &[0x1f, 0x9a, 0x0a]), (42, 8, &[0x2a])];
    for (value, n, want) in cases {
        let mut out = AVec::new_in(alloc());
        put_int(&mut out, 0, n, value).expect("put");
        assert_eq!(bytes(&out), want, "{value} with a {n}-bit prefix");
        let mut at = 0;
        assert_eq!(get_int(want, &mut at, n), Ok(value));
        assert_eq!(at, want.len());
    }
    // Past u32: refused, never wrapped (section 5.1).
    let mut at = 0;
    assert_eq!(get_int(&[0x1f, 0xff, 0xff, 0xff, 0xff, 0x7f], &mut at, 5), Err(HpackFault::IntegerOverflow));
    let mut at = 0;
    assert_eq!(get_int(&[0x1f, 0x80], &mut at, 5), Err(HpackFault::Truncated));
}

/// C.2.1: a literal with indexing, a new name.
#[test]
fn c2_1_literal_with_indexing() {
    let block = cat(&[&[0x40, 0x0a], b"custom-key", &[0x0d], b"custom-header"]);
    sequence(
        "C.2.1",
        Strings::Raw,
        4096,
        &[Step {
            fields: &[("custom-key", "custom-header")],
            block,
            table: &[("custom-key", "custom-header")],
            size: 55,
        }],
    );
}

/// C.2.2: a literal without indexing, an indexed name (decoded only: our encoder indexes such a
/// field).
#[test]
fn c2_2_literal_without_indexing() {
    let block = cat(&[&[0x04, 0x0c], b"/sample/path"]);
    let mut decoder = Decoder::new(4096, alloc());
    let mut got = Fields::new_in(alloc());
    decoder.decode(&block, &mut got, u32::MAX).expect("decode");
    assert_eq!(got.get(0), Some(Field::new(b":path", b"/sample/path")));
    assert_eq!(got.len(), 1);
    assert_eq!(decoder.table().len(), 0);
}

/// C.2.3: a literal never indexed; our encoder writes a sensitive field so, and the decoder says
/// it was never indexed.
#[test]
fn c2_3_literal_never_indexed() {
    let block = cat(&[&[0x10, 0x08], b"password", &[0x06], b"secret"]);
    let mut encoder = Encoder::agreed(4096, Strings::Raw, alloc());
    let mut out = AVec::new_in(alloc());
    encoder.encode([Field::sensitive(b"password", b"secret")].into_iter(), &mut out).expect("encode");
    assert_eq!(bytes(&out), block);
    let mut decoder = Decoder::new(4096, alloc());
    let mut got = Fields::new_in(alloc());
    decoder.decode(&block, &mut got, u32::MAX).expect("decode");
    assert_eq!(got.get(0), Some(Field::sensitive(b"password", b"secret")));
    assert_eq!(decoder.table().len(), 0);
    assert_eq!(encoder.table().len(), 0);
}

/// C.2.4: an indexed field.
#[test]
fn c2_4_indexed() {
    sequence("C.2.4", Strings::Raw, 4096, &[Step { fields: &[(":method", "GET")], block: hex("82"), table: &[], size: 0 }]);
}

const R1: &[(&str, &str)] = &[(":method", "GET"), (":scheme", "http"), (":path", "/"), (":authority", "www.example.com")];
const R2: &[(&str, &str)] =
    &[(":method", "GET"), (":scheme", "http"), (":path", "/"), (":authority", "www.example.com"), ("cache-control", "no-cache")];
const R3: &[(&str, &str)] = &[
    (":method", "GET"),
    (":scheme", "https"),
    (":path", "/index.html"),
    (":authority", "www.example.com"),
    ("custom-key", "custom-value"),
];
const T1: &[(&str, &str)] = &[(":authority", "www.example.com")];
const T2: &[(&str, &str)] = &[("cache-control", "no-cache"), (":authority", "www.example.com")];
const T3: &[(&str, &str)] = &[("custom-key", "custom-value"), ("cache-control", "no-cache"), (":authority", "www.example.com")];

/// C.3: three requests without Huffman.
#[test]
fn c3_requests_without_huffman() {
    sequence(
        "C.3",
        Strings::Raw,
        4096,
        &[
            Step { fields: R1, block: cat(&[&hex("8286 8441 0f"), b"www.example.com"]), table: T1, size: 57 },
            Step { fields: R2, block: cat(&[&hex("8286 84be 5808"), b"no-cache"]), table: T2, size: 110 },
            Step {
                fields: R3,
                block: cat(&[&hex("8287 85bf 400a"), b"custom-key", &hex("0c"), b"custom-value"]),
                table: T3,
                size: 164,
            },
        ],
    );
}

/// C.4: the same requests with Huffman.
#[test]
fn c4_requests_with_huffman() {
    sequence(
        "C.4",
        Strings::Huffman,
        4096,
        &[
            Step { fields: R1, block: hex("8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff"), table: T1, size: 57 },
            Step { fields: R2, block: hex("8286 84be 5886 a8eb 1064 9cbf"), table: T2, size: 110 },
            Step {
                fields: R3,
                block: hex("8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf"),
                table: T3,
                size: 164,
            },
        ],
    );
}

const DATE1: &str = "Mon, 21 Oct 2013 20:13:21 GMT";
const DATE2: &str = "Mon, 21 Oct 2013 20:13:22 GMT";
const LOC: &str = "https://www.example.com";
const COOKIE: &str = "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1";

const S1: &[(&str, &str)] = &[(":status", "302"), ("cache-control", "private"), ("date", DATE1), ("location", LOC)];
const S2: &[(&str, &str)] = &[(":status", "307"), ("cache-control", "private"), ("date", DATE1), ("location", LOC)];
const S3: &[(&str, &str)] = &[
    (":status", "200"),
    ("cache-control", "private"),
    ("date", DATE2),
    ("location", LOC),
    ("content-encoding", "gzip"),
    ("set-cookie", COOKIE),
];
const U1: &[(&str, &str)] = &[("location", LOC), ("date", DATE1), ("cache-control", "private"), (":status", "302")];
const U2: &[(&str, &str)] = &[(":status", "307"), ("location", LOC), ("date", DATE1), ("cache-control", "private")];
const U3: &[(&str, &str)] = &[("set-cookie", COOKIE), ("content-encoding", "gzip"), ("date", DATE2)];

/// C.5: three responses without Huffman, a table of 256 bytes, with evictions.
#[test]
fn c5_responses_without_huffman() {
    sequence(
        "C.5",
        Strings::Raw,
        256,
        &[
            Step {
                fields: S1,
                block: cat(&[
                    &hex("4803"),
                    b"302",
                    &hex("5807"),
                    b"private",
                    &hex("611d"),
                    DATE1.as_bytes(),
                    &hex("6e17"),
                    LOC.as_bytes(),
                ]),
                table: U1,
                size: 222,
            },
            Step { fields: S2, block: cat(&[&hex("4803"), b"307", &hex("c1c0bf")]), table: U2, size: 222 },
            Step {
                fields: S3,
                block: cat(&[
                    &hex("88c1 611d"),
                    DATE2.as_bytes(),
                    &hex("c0 5a04"),
                    b"gzip",
                    &hex("7738"),
                    COOKIE.as_bytes(),
                ]),
                table: U3,
                size: 215,
            },
        ],
    );
}

/// C.6: the same responses with Huffman.
#[test]
fn c6_responses_with_huffman() {
    sequence(
        "C.6",
        Strings::Huffman,
        256,
        &[
            Step {
                fields: S1,
                block: hex(
                    "4882 6402 5885 aec3 771a 4b61 96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 82a6 2d1b ff6e
                     919d 29ad 1718 63c7 8f0b 97c8 e9ae 82ae 43d3",
                ),
                table: U1,
                size: 222,
            },
            Step { fields: S2, block: hex("4883 640e ffc1 c0bf"), table: U2, size: 222 },
            Step {
                fields: S3,
                block: hex(
                    "88c1 6196 d07a be94 1054 d444 a820 0595 040b 8166 e084 a62d 1bff c05a 839b d9ab 77ad 94e7
                     821d d7f2 e6c7 b335 dfdf cd5b 3960 d5af 2708 7f36 72c1 ab27 0fb5 291f 9587 3160 65c0 03ed
                     4ee5 b106 3d50 07",
                ),
                table: U3,
                size: 215,
            },
        ],
    );
}

/// The code is complete and prefix-free: its Kraft sum is exactly 1, and no code is a prefix of
/// another. Some codes the Appendix prints, checked by value.
#[test]
fn the_huffman_code_is_appendix_b() {
    let kraft: u64 = LENS.iter().map(|&len| 1u64 << (30 - u32::from(len))).sum();
    assert_eq!(kraft, 1 << 30, "the code is not complete");
    for a in 0..257 {
        for b in a + 1..257 {
            let (ca, la) = (CANON.codes[a], u32::from(LENS[a]));
            let (cb, lb) = (CANON.codes[b], u32::from(LENS[b]));
            let short = la.min(lb);
            assert_ne!(ca >> (la - short), cb >> (lb - short), "code {a} is a prefix of {b} or the other way");
        }
    }
    let code = |s: usize| (CANON.codes[s], LENS[s]);
    assert_eq!(code(usize::from(b' ')), (0x14, 6));
    assert_eq!(code(usize::from(b'0')), (0x0, 5));
    assert_eq!(code(usize::from(b'a')), (0x3, 5));
    assert_eq!(code(0), (0x1ff8, 13));
    assert_eq!(code(255), (0x3ff_ffee, 26));
    assert_eq!(code(usize::from(EOS)), (0x3fff_ffff, 30));
}

#[test]
fn every_byte_round_trips_through_huffman() {
    let all: StdVec<u8> = (0..=255).collect();
    let mut coded = AVec::new_in(alloc());
    huffman::encode(&all, &mut coded).expect("encode");
    assert_eq!(coded.len(), huffman::encoded_len(&all));
    let mut back = AVec::new_in(alloc());
    huffman::decode(coded.as_slice(), &mut back).expect("decode");
    assert_eq!(bytes(&back), all);
}

/// Section 5.2's three refusals.
#[test]
fn bad_huffman_is_refused() {
    use crate::hpack::Halt;
    let refused = |input: &[u8]| {
        let mut out = AVec::new_in(alloc());
        huffman::decode(input, &mut out).err()
    };
    // '0' is 00000; padded with zeros, not EOS's ones.
    assert_eq!(refused(&[0b0000_0000]), Some(Halt::Fault(HpackFault::HuffmanPadding)));
    // A whole symbol then eight one bits: padding longer than 7 bits.
    assert_eq!(refused(&[0b0000_0111, 0xff]), Some(Halt::Fault(HpackFault::HuffmanPadding)));
    // EOS (30 ones) then two ones of padding.
    assert_eq!(refused(&[0xff, 0xff, 0xff, 0xff]), Some(Halt::Fault(HpackFault::HuffmanEos)));
    // The empty string is no error.
    assert_eq!(refused(&[]), None);
}

/// RFC 7541 Appendix A, a sample of its 61 entries by index (TO VERIFY AGAINST THE RFC TEXT:
/// written from memory of the Appendix).
#[test]
fn the_static_table_is_appendix_a() {
    let at = |i: usize| (core::str::from_utf8(STATIC[i - 1].0).unwrap(), core::str::from_utf8(STATIC[i - 1].1).unwrap());
    assert_eq!(STATIC.len(), 61);
    assert_eq!(at(1), (":authority", ""));
    assert_eq!(at(2), (":method", "GET"));
    assert_eq!(at(7), (":scheme", "https"));
    assert_eq!(at(8), (":status", "200"));
    assert_eq!(at(14), (":status", "500"));
    assert_eq!(at(16), ("accept-encoding", "gzip, deflate"));
    assert_eq!(at(24), ("cache-control", ""));
    assert_eq!(at(28), ("content-length", ""));
    assert_eq!(at(32), ("cookie", ""));
    assert_eq!(at(33), ("date", ""));
    assert_eq!(at(38), ("host", ""));
    assert_eq!(at(46), ("location", ""));
    assert_eq!(at(55), ("set-cookie", ""));
    assert_eq!(at(61), ("www-authenticate", ""));
    for (name, _) in STATIC {
        assert!(name.iter().all(|b| !b.is_ascii_uppercase()), "an upper-case static name");
    }
}

/// A size update after a field, more than two, or above the allowed size, is refused (section
/// 4.2, 6.3); one owed after the maximum was lowered must come first.
#[test]
fn size_updates_are_checked() {
    use crate::hpack::Halt;
    let decode = |decoder: &mut Decoder<'_>, block: &[u8]| {
        let mut got = Fields::new_in(alloc());
        decoder.decode(block, &mut got, u32::MAX).err()
    };
    let mut d = Decoder::new(4096, alloc());
    assert_eq!(decode(&mut d, &hex("82 3f e1 1f")), Some(Halt::Fault(HpackFault::SizeUpdateLate)));
    let mut d = Decoder::new(4096, alloc());
    assert_eq!(decode(&mut d, &hex("20 20 20")), Some(Halt::Fault(HpackFault::SizeUpdatesOverTwo)));
    let mut d = Decoder::new(100, alloc());
    assert_eq!(decode(&mut d, &hex("3f 46")), Some(Halt::Fault(HpackFault::SizeOverMax { size: 101, most: 100 })));
    let mut d = Decoder::new(4096, alloc());
    d.set_allowed(100);
    assert_eq!(decode(&mut d, &hex("82")), Some(Halt::Fault(HpackFault::SizeUpdateMissing)));
    let mut d = Decoder::new(4096, alloc());
    d.set_allowed(100);
    assert_eq!(decode(&mut d, &hex("3f 45 82")), None);
    assert_eq!(d.table().max(), 100);
    let mut d = Decoder::new(4096, alloc());
    assert_eq!(decode(&mut d, &hex("80")), Some(Halt::Fault(HpackFault::IndexZero)));
    assert_eq!(decode(&mut d, &hex("be")), Some(Halt::Fault(HpackFault::IndexOutOfRange { index: 62, entries: 61 })));
}
