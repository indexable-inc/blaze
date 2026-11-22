//! `BLAZE_LAUNCH_GRANTS` (crate::grants): what the parent writes reads back in the launcher as
//! the same pairs in the same order, and what is not of the form is refused, naming the grant.

use blaze_sim::{Config, Tape};

use crate::grants::{GrantsError, MOST_BYTES, MOST_GRANTS, MOST_PART, Part, format, parse};

/// The bytes a part is drawn from: every one the form allows.
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";

fn part(tape: &Tape, label: &'static str) -> String {
    let len: usize = tape.int_in(1..=MOST_PART, label);
    (0..len).map(|_| ALPHABET[tape.int_in(0..=ALPHABET.len() - 1, "byte")] as char).collect()
}

/// Invariant: any list of distinct pairs of the form, up to [`MOST_GRANTS`] of parts up to
/// [`MOST_PART`] bytes, is written within [`MOST_BYTES`] and read back as the same pairs in the
/// same order. Prints nothing; passes as `grants_round_trip ... ok`.
#[test]
fn grants_round_trip() {
    blaze_sim::check(Config::cases(400), |tape| {
        let count: usize = tape.int_in(0..=MOST_GRANTS, "grants");
        let mut owned: Vec<(String, String)> = Vec::new();
        for index in 0..count {
            let kind = part(tape, "kind");
            // Distinct by construction: a pair drawn twice is the refusal tested below.
            let mut name = part(tape, "name");
            if owned.iter().any(|(k, n)| *k == kind && *n == name) {
                name.truncate(MOST_PART - 3);
                name.push_str(&format!("{:03}", index % 1000));
                if owned.iter().any(|(k, n)| *k == kind && *n == name) {
                    continue;
                }
            }
            owned.push((kind, name));
        }
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, n)| (k.as_str(), n.as_str())).collect();
        let mut out = [0u8; MOST_BYTES];
        let text = format(&pairs, &mut out).unwrap_or_else(|why| panic!("{pairs:?} was refused: {why}"));
        let read = parse(text.as_bytes()).unwrap_or_else(|why| panic!("`{text}` did not read: {why}"));
        let back: Vec<(&str, &str)> = read.iter().collect();
        assert_eq!(back, pairs, "`{text}` read back as other pairs");
        assert_eq!(read.len(), pairs.len());
    });
}

/// The empty text is no grant, and one grant is `<kind>=<name>` exactly as the parent writes
/// it: the form lane S sets.
#[test]
fn the_form_is_kind_equals_name_joined_by_commas() {
    let mut out = [0u8; MOST_BYTES];
    assert_eq!(format(&[], &mut out), Ok(""));
    assert!(parse(b"").expect("no grant").is_empty());
    assert_eq!(format(&[("whole", "files"), ("whole", "net_2")], &mut out), Ok("whole=files,whole=net_2"));
}

/// Each malformed value is refused whole, naming the grant (from 1) and the part; and the
/// writer refuses what the reader would.
#[test]
fn a_malformed_value_is_refused_by_name() {
    let refusals: [(&[u8], GrantsError); 9] = [
        (b"whole", GrantsError::NoEquals { nth: 1 }),
        (b"whole=a,", GrantsError::NoEquals { nth: 2 }),
        (b"=files", GrantsError::Empty { nth: 1, part: Part::Kind }),
        (b"whole=", GrantsError::Empty { nth: 1, part: Part::Name }),
        (b"whole=a=b", GrantsError::Character { nth: 1, part: Part::Name, byte: b'=' }),
        (b"whole=a,who le=b", GrantsError::Character { nth: 2, part: Part::Kind, byte: b' ' }),
        (b"whole=a,whole=a", GrantsError::Twice { nth: 2, first: 1 }),
        (b"whole=\xff", GrantsError::NotText),
        ("whole=caf\u{e9}".as_bytes(), GrantsError::Character { nth: 1, part: Part::Name, byte: 0xc3 }),
    ];
    for (value, refusal) in refusals {
        assert_eq!(parse(value).map(|it| it.len()), Err(refusal), "{:?}", String::from_utf8_lossy(value));
    }
    let long = "a".repeat(MOST_PART + 1);
    let mut out = [0u8; MOST_BYTES];
    assert_eq!(
        format(&[("whole", &long)], &mut out),
        Err(GrantsError::PartTooLong { nth: 1, part: Part::Name, len: MOST_PART + 1 })
    );
    assert_eq!(format(&[("whole", "a"), ("whole", "a")], &mut out), Err(GrantsError::Twice { nth: 2, first: 1 }));
    assert_eq!(format(&[("whole", "a,b")], &mut out), Err(GrantsError::Character { nth: 1, part: Part::Name, byte: b',' }));
    let many: Vec<String> = (0..=MOST_GRANTS).map(|at| format!("p{at}")).collect();
    let pairs: Vec<(&str, &str)> = many.iter().map(|name| ("whole", name.as_str())).collect();
    assert_eq!(format(&pairs, &mut out), Err(GrantsError::TooMany { most: MOST_GRANTS }));
    let joined = pairs.iter().map(|(k, n)| format!("{k}={n}")).collect::<Vec<_>>().join(",");
    assert_eq!(parse(joined.as_bytes()).map(|it| it.len()), Err(GrantsError::TooMany { most: MOST_GRANTS }));
    let too_long = vec![b'a'; MOST_BYTES + 1];
    assert_eq!(parse(&too_long).map(|it| it.len()), Err(GrantsError::TooLong { len: MOST_BYTES + 1, most: MOST_BYTES }));
}
