//! The golden vectors (tools/world/vectors.json), which the TypeScript client's test reads too:
//! for each, the bytes decode to the value its JSON names (the decoded value rendered as JSON
//! equals the vector's JSON) and that value encodes to exactly the bytes. Every call, every
//! reply and every refusal has a vector.

use blaze_data::Value;
use blaze_mem::{Frame, Tag};
use blaze_own::{Alloc, Erased, Vec};

use crate::{Limits, Refusal, Request, Response, test_support::hex};

const VECTORS: &str = include_str!("../../../tools/world/vectors.json");

fn refusal_tag(refusal: &Refusal<'_>) -> usize {
    match refusal {
        Refusal::Edit { .. } => 0,
        Refusal::NoSuchWorld { .. } => 1,
        Refusal::NotACheckout { .. } => 2,
        Refusal::NoSuchPath { .. } => 3,
        Refusal::NoSuchItem { .. } => 4,
        Refusal::NoSuchFinding { .. } => 5,
        Refusal::Wire { .. } => 6,
        Refusal::Io { .. } => 7,
        Refusal::Unsupported { .. } => 8,
    }
}

fn index(code: u64) -> usize {
    match usize::try_from(code) {
        Ok(code) => code,
        Err(_) => unreachable!("a code of the schema is small"),
    }
}

/// One vector: checked, and which call, reply or refusal it covers marked.
fn check_one(
    vector: &Value<Alloc<'_>>,
    alloc: Alloc<'_>,
    calls: &mut [bool; 7],
    replies: &mut [bool; 9],
    refusals: &mut [bool; 9],
) {
    let name = vector.get("name").and_then(Value::as_str).expect("a vector has a name");
    let json = vector.get("json").unwrap_or_else(|| panic!("{name}: no json"));
    let cbor = vector.get("cbor_hex").and_then(Value::as_str).unwrap_or_else(|| panic!("{name}: no cbor_hex"));
    let bytes = hex(cbor, alloc);
    let mut out = Vec::new_in(alloc);
    if json.get("call").is_some() {
        let request = Request::decode(&bytes, Limits::DEFAULT, alloc).unwrap_or_else(|error| panic!("{name}: {error}"));
        let rendered = request.to_value(alloc);
        assert!(rendered == *json, "{name}: the bytes decode to {rendered:?}, the vector's JSON is {json:?}");
        request.encode(&mut out);
        assert_eq!(out.as_slice(), bytes.as_slice(), "{name}: encoded");
        calls[index(request.call().code())] = true;
    } else {
        let response = Response::decode(&bytes, Limits::DEFAULT, alloc).unwrap_or_else(|error| panic!("{name}: {error}"));
        let rendered = response.to_value(alloc);
        assert!(rendered == *json, "{name}: the bytes decode to {rendered:?}, the vector's JSON is {json:?}");
        response.encode(&mut out);
        assert_eq!(out.as_slice(), bytes.as_slice(), "{name}: encoded");
        replies[index(response.reply_code())] = true;
        if let Response::Refused { refusal } = &response {
            refusals[refusal_tag(refusal)] = true;
        }
    }
}

#[test]
fn every_vector_decodes_to_its_json_and_encodes_to_its_bytes() {
    let frame = Frame::with_reserve(1usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let all = blaze_json::read_in(VECTORS.as_bytes(), blaze_json::Limits::DEFAULT, alloc).expect("tools/world/vectors.json is JSON");
    let vectors = all.as_list().expect("tools/world/vectors.json is an array");
    let (mut calls, mut replies, mut refusals) = ([false; 7], [false; 9], [false; 9]);
    for vector in vectors {
        check_one(vector, alloc, &mut calls, &mut replies, &mut refusals);
    }
    assert!(calls.iter().all(|it| *it), "a call without a vector: {calls:?}");
    assert!(replies.iter().all(|it| *it), "a reply without a vector: {replies:?}");
    assert!(refusals.iter().all(|it| *it), "a refusal without a vector: {refusals:?}");
}
