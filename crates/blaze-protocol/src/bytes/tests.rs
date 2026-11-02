use serde::{Deserialize, Serialize};

use super::{Buf, Bytes};
use crate::cbor::{self, Error};

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct Held {
    #[serde(with = "crate::bytes")]
    data: Vec<u8>,
}

/// The same field as serde writes it by default: the old encoding, kept as the reference.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct Plain {
    data: Vec<u8>,
}

#[derive(Deserialize, PartialEq, Eq, Debug)]
struct Files {
    files: Vec<(String, Buf)>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

/// The head of a map of one key, `data`.
const DATA: &str = "a16464617461";
/// Its length in bytes.
const DATA_LEN: usize = DATA.len() / 2;

#[test]
fn bytes_are_one_byte_string() {
    let held = |data: &[u8]| Held { data: data.to_vec() };
    let ff = "ff".repeat(24);
    let cases: [(&[u8], String); 3] = [
        (&[], "40".to_owned()),
        (&[1, 2, 3], "43010203".to_owned()),
        (&[0xff; 24], format!("5818{ff}")),
    ];
    for (data, item) in cases {
        let bytes = cbor::to_vec(&held(data)).unwrap();
        assert_eq!(hex(&bytes), format!("{DATA}{item}"));
        assert_eq!(cbor::from_slice::<Held>(&bytes).unwrap(), held(data));
    }
    let long = vec![7; 300];
    let bytes = cbor::to_vec(&held(&long)).unwrap();
    assert_eq!(hex(&bytes[..DATA_LEN + 3]), format!("{DATA}59012c"));
    assert_eq!(bytes.len(), DATA_LEN + 3 + 300);
    assert_eq!(cbor::from_slice::<Held>(&bytes).unwrap().data, long);
}

/// The array of the old encoding is a second encoding of the same value, so it is refused.
#[test]
fn an_array_of_bytes_is_refused() {
    let old = cbor::to_vec(&Plain { data: vec![1, 2, 3] }).unwrap();
    assert_eq!(hex(&old), format!("{DATA}83010203"));
    match cbor::from_slice::<Held>(&old) {
        Err(Error::Message(said)) => assert!(said.contains("expected a byte string"), "{said}"),
        other => panic!("{other:?}"),
    }
}

/// Seeded bytes read back the same through the byte string as through the old array, and the
/// byte string is the bytes behind one head.
#[test]
fn bytes_read_back_as_the_old_array_did() {
    let mut state = 0x853c_49e6_748f_ea9b_u64;
    for round in 0..500 {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        let len = match round % 5 {
            0 => (state >> 40) as usize % 70_000,
            _ => (state >> 40) as usize % 300,
        };
        let data: Vec<u8> = (0..len).map(|at| (state >> (at % 57)) as u8 ^ at as u8).collect();
        let new = cbor::to_vec(&Held { data: data.clone() }).unwrap();
        let old = cbor::to_vec(&Plain { data: data.clone() }).unwrap();
        assert_eq!(&new[new.len() - len..], &data[..]);
        assert_eq!(new[DATA_LEN] >> 5, 2, "a byte string's major type");
        assert!(new.len() <= old.len());
        let back = cbor::from_slice::<Held>(&new).unwrap().data;
        assert_eq!(back, cbor::from_slice::<Plain>(&old).unwrap().data);
        assert_eq!(back, data);
        assert_eq!(cbor::reference::to_vec(&Held { data: data.clone() }).unwrap(), new);
        assert_eq!(cbor::reference::from_slice::<Held>(&new).unwrap().data, data);
    }
}

/// serde_json writes bytes as the array it wrote the sequence as, and reads either back.
#[test]
fn json_is_as_it_was() {
    let data = vec![0, 1, 127, 255];
    let new = serde_json::to_string(&Held { data: data.clone() }).unwrap();
    assert_eq!(new, serde_json::to_string(&Plain { data: data.clone() }).unwrap());
    assert_eq!(new, r#"{"data":[0,1,127,255]}"#);
    assert_eq!(serde_json::from_str::<Held>(&new).unwrap().data, data);
}

#[test]
fn a_list_of_pairs_holds_byte_strings() {
    let files = Files { files: vec![("a".to_owned(), Buf(vec![1, 2]))] };
    #[derive(Serialize)]
    struct Lent<'a> {
        files: Vec<(&'a str, Bytes<'a>)>,
    }
    let lent = Lent { files: vec![("a", Bytes(&[1, 2]))] };
    let bytes = cbor::to_vec(&lent).unwrap();
    assert_eq!(hex(&bytes), "a16566696c657381826161420102");
    assert_eq!(cbor::from_slice::<Files>(&bytes).unwrap(), files);
}
