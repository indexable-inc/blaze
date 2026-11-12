//! The certificate's record: its parts' bytes, its keys and the local evidence beside it.
//! The check and the write are tested against a model in `request/tests.rs`.

use super::*;

fn id(n: u8) -> ContentId {
    ContentId::of("test certificate", &[n])
}

fn reads() -> Vec<(Read, Digest)> {
    vec![
        (Read::Outside("/usr/include/z.h".to_owned()), Digest::Nothing),
        (Read::Env("CC".to_owned()), Digest::Is(id(1))),
        (Read::Node("crates/a".to_owned()), Digest::Is(id(2))),
        (Read::Drawn, Digest::Is(id(3))),
        (Read::Listing("/opt/lib".to_owned()), Digest::Is(id(4))),
        (Read::Node(String::new()), Digest::Is(id(9))),
    ]
}

fn answer() -> Answer {
    Answer {
        outputs: vec![
            Output { base: 1, path: "debug/b".to_owned(), content: id(5) },
            Output { base: 0, path: "debug/a".to_owned(), content: id(6) },
        ],
        units: vec![id(7), id(8)],
        said: b"said".to_vec(),
    }
}

fn shape() -> Shape {
    Shape { plan: id(20), config: id(21), request: id(22) }
}

/// Rule 26: an answer's bytes read back are the answer, and encode to the same bytes.
#[test]
fn an_answer_round_trips_with_identical_bytes() {
    let certificate = Certificate::new(reads(), answer()).unwrap();
    let bytes = certificate.answer().encode();
    let back = Answer::decode(&bytes).unwrap();
    assert_eq!(&back, certificate.answer());
    assert_eq!(back.encode(), bytes);
    assert_eq!(Answer::decode(&Answer::default().encode()).unwrap(), Answer::default());
}

/// Rule 26: an answer and a list of a layout before this one are refused by that layout's
/// name, one of a layout not known by its number, other bytes as no certificate; none is
/// read as an empty record.
#[test]
fn another_layout_is_refused_by_name() {
    let bytes = Certificate::new(reads(), answer()).unwrap().answer().encode();
    for (layout, _) in PREVIOUS {
        let mut old = bytes.clone();
        old[8..12].copy_from_slice(&layout.to_le_bytes());
        match Answer::decode(&old) {
            Err(why) => {
                assert!(matches!(why, Undecoded::Previous(n, _) if n == *layout), "{why:?}");
                assert!(why.to_string().contains(&format!("layout {layout} (")), "{why}");
            }
            Ok(read) => panic!("an answer of layout {layout} was read: {read:?}"),
        }
        let base = base(&shape(), b"blaze 1");
        let mut list = Vec::new();
        blaze_record::write_list(&mut list, &base, &head_of(*layout), [Read::Drawn.name()]);
        let opened = ListRef::open(&list, &base).unwrap();
        assert_eq!(reads_of(opened), Err(Undecoded::Previous(*layout, PREVIOUS[*layout as usize].1)));
    }
    let mut later = bytes.clone();
    later[8..12].copy_from_slice(&77_u32.to_le_bytes());
    assert_eq!(Answer::decode(&later), Err(Undecoded::Unknown(77)));
    assert_eq!(Answer::decode(b"not a certificate"), Err(Undecoded::NotOne));
    assert_eq!(Answer::decode(b""), Err(Undecoded::NotOne));
}

/// Every cut of an answer's bytes, and bytes past its end, are refused at a field.
#[test]
fn every_cut_is_refused() {
    let mut bytes = Certificate::new(reads(), answer()).unwrap().answer().encode();
    for cut in 0..bytes.len() {
        assert!(Answer::decode(&bytes[..cut]).is_err(), "a cut at {cut} was read");
    }
    bytes.push(0);
    assert_eq!(Answer::decode(&bytes), Err(Undecoded::Malformed("the end")));
}

/// Rule 25: equal builds are equal bytes whatever order their steps read and placed in: the
/// list names the reads sorted, each once, with no value; the outputs are sorted.
#[test]
fn equal_builds_are_equal_bytes_in_any_order() {
    let base = base(&shape(), b"blaze 1");
    let one = Certificate::new(reads(), answer()).unwrap();
    let mut turned = reads();
    turned.reverse();
    turned.push(turned[0].clone());
    let mut placed = answer();
    placed.outputs.reverse();
    let two = Certificate::new(turned, placed).unwrap();
    let list = one.list(&base);
    assert_eq!(list, two.list(&base));
    assert_eq!(one.answer_key(&list), two.answer_key(&list));
    assert_eq!(one.answer().encode(), two.answer().encode());
    let named = reads_of(ListRef::open(&list, &base).unwrap()).unwrap();
    let mut sorted: Vec<Read> = reads().into_iter().map(|(read, _)| read).collect();
    sorted.sort();
    assert_eq!(named, sorted);
    // A value is in the answer's key, not in the list.
    let mut moved = reads();
    moved[1].1 = Digest::Is(id(99));
    let three = Certificate::new(moved, answer()).unwrap();
    assert_eq!(three.list(&base), list);
    assert_ne!(three.answer_key(&list), one.answer_key(&list));
    let mut gone = reads();
    gone[1].1 = Digest::Nothing;
    let four = Certificate::new(gone, answer()).unwrap();
    assert_ne!(four.answer_key(&list), one.answer_key(&list));
}

/// A read seen at two values has no one digest: that build has no certificate, by the
/// read's name.
#[test]
fn a_read_at_two_values_is_refused_by_name() {
    let mut twice = reads();
    twice.push((Read::Env("CC".to_owned()), Digest::Nothing));
    assert_eq!(Certificate::new(twice, answer()), Err(Read::Env("CC".to_owned())));
}

/// The base is the request's shape and the tool: each part of either gives another base.
/// The tree's content is no parameter of it.
#[test]
fn the_base_is_the_shape_and_the_tool() {
    let one = base(&shape(), b"blaze 1");
    assert_eq!(one, base(&shape(), b"blaze 1"));
    assert_ne!(one, base(&shape(), b"blaze 2"));
    assert_ne!(one, base(&Shape { plan: id(0), ..shape() }, b"blaze 1"));
    assert_ne!(one, base(&Shape { config: id(0), ..shape() }, b"blaze 1"));
    assert_ne!(one, base(&Shape { request: id(0), ..shape() }, b"blaze 1"));
}

/// A read's name reads back as the read; other bytes are refused.
#[test]
fn a_read_round_trips_by_name() {
    for (read, _) in reads() {
        assert_eq!(Read::named(&read.name()), Ok(read));
    }
    assert!(Read::named(b"").is_err());
    assert!(Read::named(b"\x09x").is_err());
    assert!(Read::named(b"\x04x").is_err());
    assert!(Read::named(b"\x00\xff").is_err());
}

/// Two checkouts of one content make one text of each path, and each gets its own back.
#[test]
fn the_checkout_is_relocated_and_put_back() {
    let (a, b) = ("/home/a/checkout", "/srv/b");
    let text = |root: &str| format!("{root}/target --cfg=x:{root} \"{root}/src\" PATH={root}/bin:/usr/bin {root}");
    assert_eq!(relocated(&text(a), a), relocated(&text(b), b));
    assert!(!relocated(&text(a), a).contains(a));
    // A longer name that begins as the checkout's does is another directory.
    assert_eq!(relocated("/home/a/checkout-other/x", a), "/home/a/checkout-other/x");
    assert_eq!(relocated("in/home/a/checkout/x", a), "in/home/a/checkout/x");
    let said = format!("{a}/target/debug/app");
    assert_eq!(replaced(&relocated(&said, a), b), "/srv/b/target/debug/app");
    assert_eq!(relocated("x", ""), "x");
}

/// A stamp vouches for the content hashed under it only when it was seen [`RACY_NS`] past
/// its change time; a closer look keeps nothing and drops what was kept.
#[test]
fn a_stamp_seen_too_close_to_its_change_does_not_vouch() {
    let mut evidence = Evidence::default();
    let stamp = Stamp { len: 4, changed_ns: 10 * RACY_NS };
    evidence.keep(0, "debug/a", stamp, id(1), stamp.changed_ns + RACY_NS - 1);
    assert_eq!(evidence.vouched(0, "debug/a", stamp), None);
    evidence.keep(0, "debug/a", stamp, id(1), stamp.changed_ns + RACY_NS);
    assert_eq!(evidence.vouched(0, "debug/a", stamp), Some(id(1)));
    assert_eq!(evidence.vouched(1, "debug/a", stamp), None);
    assert_eq!(evidence.vouched(0, "debug/a", Stamp { len: 5, ..stamp }), None);
    let later = Stamp { len: 4, changed_ns: 11 * RACY_NS };
    evidence.keep(0, "debug/a", later, id(2), later.changed_ns);
    assert_eq!(evidence.vouched(0, "debug/a", stamp), None);
    assert_eq!(evidence.vouched(0, "debug/a", later), None);
    // The bound: only the last answer's outputs stay.
    evidence.keep(0, "debug/a", stamp, id(1), u64::MAX);
    evidence.keep(1, "debug/b", stamp, id(1), u64::MAX);
    evidence.only(&[Output { base: 1, path: "debug/b".to_owned(), content: id(1) }]);
    assert_eq!(evidence.vouched(0, "debug/a", stamp), None);
    assert_eq!(evidence.vouched(1, "debug/b", stamp), Some(id(1)));
    evidence.forget(1, "debug/b");
    assert_eq!(evidence.vouched(1, "debug/b", stamp), None);
}
