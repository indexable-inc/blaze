//! The published vectors, on the public surface: the 12 cases of "Taming the many EdDSAs" and
//! Project Wycheproof's 210 Ed25519 vectors (`data/README.md` names their sources, revisions
//! and licenses), as `data/published.txt` lines that `data/convert.py` wrote from the vendored
//! JSON files. Each is checked for its exact verdict: `Ok`, or the reason it is refused.
//!
//! A signature that is not 64 bytes never reaches the crate: [`verify`] takes `&[u8; 64]`, so
//! the length is checked where a caller's slice becomes that type, and the test does exactly
//! that conversion. Under Miri only the paper's 12 run (minutes); the whole file, Wycheproof's
//! 210 with them, is hours there and is left to the native run.

use blaze_ed25519::{Refused, verify};

const PUBLISHED: &str = include_str!("data/published.txt");

fn bytes(hex: &str) -> Vec<u8> {
    if hex == "-" {
        return Vec::new();
    }
    assert!(hex.len() % 2 == 0, "hex of odd length: {hex}");
    (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex")).collect()
}

/// A verdict as `published.txt` writes it.
fn said(verdict: Result<(), Refused>) -> String {
    match verdict {
        Ok(()) => "Ok".to_owned(),
        Err(Refused::KeyEncoding(why)) => format!("KeyEncoding.{why:?}"),
        Err(Refused::REncoding(why)) => format!("REncoding.{why:?}"),
        Err(why) => format!("{why:?}"),
    }
}

#[test]
#[cfg_attr(miri, ignore = "Wycheproof's 210 vectors are hours under Miri; the paper's 12 run there in the_papers_cases_by_number")]
fn the_published_vectors() {
    let (mut speccheck, mut valid, mut invalid, mut acceptable, mut by_length) = (0, 0, 0, 0, 0);
    for line in PUBLISHED.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let [source, id, label, key, message, signature, want] = fields[..] else { panic!("a line of seven fields: {line}") };
        let name = format!("{source} {id} ({label})");
        let key: [u8; 32] = bytes(key).try_into().unwrap_or_else(|_| panic!("{name}: a key of 32 bytes"));
        let message = bytes(message);
        let signature = bytes(signature);
        let got = match <&[u8; 64]>::try_from(&signature[..]) {
            Ok(signature) => said(verify(&key, &message, signature)),
            Err(_) => {
                by_length += 1;
                "Length".to_owned()
            }
        };
        assert_eq!(got, want, "{name}");
        // The source's own verdict against blaze's: every valid one accepted, everything else
        // refused, and every "acceptable" one refused for its small-order or mixed-order key.
        match (source, label) {
            ("speccheck", "case") => {
                speccheck += 1;
                assert_ne!(got, "Ok", "{name}: the paper's cases are all refused");
            }
            ("wycheproof", "valid") => {
                valid += 1;
                assert_eq!(got, "Ok", "{name}");
            }
            ("wycheproof", "invalid") => {
                invalid += 1;
                assert_ne!(got, "Ok", "{name}");
            }
            ("wycheproof", "acceptable") => {
                acceptable += 1;
                assert!(got == "KeySmallOrder" || got == "KeyMixedOrder", "{name}: {got}");
            }
            _ => panic!("{name}: an unknown source or label"),
        }
    }
    assert_eq!((speccheck, valid, invalid, acceptable, by_length), (12, 117, 64, 29, 12), "every vector was read");
}

/// The paper's 12, by number: each verified, and refused for the reason the table `convert.py`
/// asserts. Runs under Miri too.
#[test]
fn the_papers_cases_by_number() {
    let want = [
        "KeySmallOrder",
        "KeySmallOrder",
        "KeyMixedOrder",
        "KeyMixedOrder",
        "KeyMixedOrder",
        "KeyMixedOrder",
        "SNotBelowOrder",
        "SNotBelowOrder",
        "KeyMixedOrder",
        "KeyMixedOrder",
        "KeyEncoding.NegativeZero",
        "KeyEncoding.NegativeZero",
    ];
    let cases: Vec<&str> = PUBLISHED.lines().filter(|line| line.starts_with("speccheck ")).collect();
    assert_eq!(cases.len(), 12);
    for (number, line) in cases.iter().enumerate() {
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields[1], number.to_string(), "in the paper's order");
        assert_eq!(fields[6], want[number], "case {number}");
        let key: [u8; 32] = bytes(fields[3]).try_into().unwrap_or_else(|_| panic!("case {number}: a key of 32 bytes"));
        let signature: [u8; 64] = bytes(fields[5]).try_into().unwrap_or_else(|_| panic!("case {number}: a signature of 64 bytes"));
        assert_eq!(said(verify(&key, &bytes(fields[4]), &signature)), want[number], "case {number}, verified");
    }
}
