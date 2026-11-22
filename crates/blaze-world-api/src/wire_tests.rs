//! The wire: the round trip as a property over generated requests and responses; every limit
//! one over refused by name and exactly at it accepted; each deterministic-encoding rule and
//! each schema rule refused by name.

use std::format;

use blaze_mem::{Frame, Tag};
use blaze_own::{AString, AVec, Alloc, Erased, Vec};
use blaze_sim::{Broke, Config, Tape};

use crate::{
    Answer, Call, Change, Edit, EditWhy, Entered, Entry, EntryKind, Finding, FindingId, Limits, MOST_INT, Note, Place, Refusal,
    Request, Response, Spot, Subject, Target, WireCode, WireError, What, WorldId,
    test_support::{checkout, hex, item, list, path, text, world},
};

// ---------------------------------------------------------------------------------------------
// Draws.

const SEGMENTS: &[&str] = &["a", "src", "lib.rs", "mod.rs", "é", "a b", "x-y_z"];

fn draw_path<'a>(tape: &Tape, alloc: Alloc<'a>) -> crate::Path<'a> {
    let mut joined = AString::new_in(alloc);
    for (i, segment) in tape.vec(1..=3, "segments", |t| *t.one_of(SEGMENTS, "segment")).into_iter().enumerate() {
        if i > 0 {
            joined.push_str("/");
        }
        joined.push_str(segment);
    }
    path(joined.as_str(), alloc)
}

fn draw_text<'a>(tape: &Tape, label: &'static str, alloc: Alloc<'a>) -> AString<'a> {
    text(&tape.text(0..=12, label), alloc)
}

fn draw_id(tape: &Tape, label: &'static str) -> [u8; 32] {
    tape.array::<u8, 32>(label, |t| t.int_in(0u8..=u8::MAX, "byte"))
}

fn draw_world(tape: &Tape) -> WorldId {
    WorldId(draw_id(tape, "world"))
}

fn draw_item(tape: &Tape) -> crate::ItemKey {
    crate::ItemKey(blaze_id::ContentId(draw_id(tape, "item")))
}

fn draw_int(tape: &Tape, label: &'static str) -> u64 {
    match tape.coin("small") {
        true => tape.int_in(0..=30u64, label),
        false => tape.int_in(0..=MOST_INT, label),
    }
}

fn draw_list<'a, T>(tape: &Tape, label: &'static str, alloc: Alloc<'a>, each: impl FnMut(&Tape) -> T) -> AVec<'a, T> {
    list(tape.vec(0..=3, label, each), alloc)
}

fn draw_edit<'a>(tape: &Tape, alloc: Alloc<'a>) -> Edit<'a> {
    match tape.int_in(0u8..=4, "edit") {
        0 => Edit::WriteFile { path: draw_path(tape, alloc), text: draw_text(tape, "text", alloc) },
        1 => Edit::DeleteFile { path: draw_path(tape, alloc) },
        2 => Edit::RenameFile { from: draw_path(tape, alloc), to: draw_path(tape, alloc) },
        3 => {
            let start = draw_int(tape, "start");
            let end = start + tape.int_in(0..=MOST_INT - start, "len");
            Edit::ReplaceRange {
                path: draw_path(tape, alloc),
                start,
                end,
                text: draw_text(tape, "text", alloc),
                expect: draw_text(tape, "expect", alloc),
            }
        }
        _ => Edit::ReplaceItem { item: draw_item(tape), text: draw_text(tape, "text", alloc) },
    }
}

fn draw_request<'a>(tape: &Tape, alloc: Alloc<'a>) -> Request<'a> {
    match tape.int_in(0u8..=6, "call") {
        0 => Request::Open { checkout: checkout(&format!("/{}", draw_path(tape, alloc).as_str()), alloc) },
        1 => Request::Patch { base: draw_world(tape), edits: draw_list(tape, "edits", alloc, |t| draw_edit(t, alloc)) },
        2 => Request::Read {
            world: draw_world(tape),
            what: match tape.int_in(0u8..=2, "what") {
                0 => What::File { path: draw_path(tape, alloc) },
                1 => What::Item { key: draw_item(tape) },
                _ => What::Dir { path: tape.option("dir", |t| draw_path(t, alloc)) },
            },
        },
        3 => Request::Check {
            world: draw_world(tape),
            select: draw_list(tape, "select", alloc, |t| match t.coin("target") {
                true => Target::Path { path: draw_path(t, alloc) },
                false => Target::Item { key: draw_item(t) },
            }),
            lints: tape.coin("lints"),
        },
        4 => Request::Diff { from: draw_world(tape), to: draw_world(tape) },
        5 => Request::Commit {
            world: draw_world(tape),
            onto: checkout(&format!("/{}", draw_path(tape, alloc).as_str()), alloc),
            expect: draw_world(tape),
        },
        _ => Request::Explain {
            world: draw_world(tape),
            subject: match tape.coin("subject") {
                true => Subject::Finding { id: FindingId(draw_id(tape, "finding")) },
                false => Subject::Item { key: draw_item(tape) },
            },
        },
    }
}

fn draw_spot<'a>(tape: &Tape, alloc: Alloc<'a>) -> Spot<'a> {
    let start = draw_int(tape, "start");
    let end = start + tape.int_in(0..=MOST_INT - start, "len");
    Spot {
        item: draw_item(tape),
        path: draw_path(tape, alloc),
        place: Place { start, end, line: draw_int(tape, "line"), column: draw_int(tape, "column") },
    }
}

fn draw_finding<'a>(tape: &Tape, alloc: Alloc<'a>) -> Finding<'a> {
    Finding {
        id: FindingId(draw_id(tape, "finding")),
        code: draw_text(tape, "code", alloc),
        message: draw_text(tape, "message", alloc),
        at: draw_spot(tape, alloc),
        notes: draw_list(tape, "notes", alloc, |t| Note {
            message: draw_text(t, "note", alloc),
            at: draw_list(t, "note spots", alloc, |t| draw_spot(t, alloc)),
        }),
        fix: draw_list(tape, "fix", alloc, |t| draw_edit(t, alloc)),
    }
}

fn draw_code<T: Copy>(tape: &Tape, all: &[T], label: &'static str) -> T {
    *tape.one_of(all, label)
}

fn draw_refusal<'a>(tape: &Tape, alloc: Alloc<'a>) -> Refusal<'a> {
    match tape.int_in(0u8..=8, "refusal") {
        0 => Refusal::Edit { index: draw_int(tape, "index"), why: draw_code(tape, EditWhy::ALL, "why") },
        1 => Refusal::NoSuchWorld { world: draw_world(tape) },
        2 => Refusal::NotACheckout { checkout: checkout(&format!("/{}", draw_path(tape, alloc).as_str()), alloc) },
        3 => Refusal::NoSuchPath { path: draw_path(tape, alloc) },
        4 => Refusal::NoSuchItem { item: draw_item(tape) },
        5 => Refusal::NoSuchFinding { id: FindingId(draw_id(tape, "finding")) },
        6 => Refusal::Wire {
            at: draw_int(tape, "at"),
            what: draw_code(tape, WireCode::ALL, "what"),
            field: draw_text(tape, "field", alloc),
            said: draw_int(tape, "said"),
        },
        7 => Refusal::Io { path: draw_path(tape, alloc), message: draw_text(tape, "message", alloc) },
        _ => Refusal::Unsupported { call: draw_code(tape, Call::ALL, "call"), why: draw_text(tape, "why", alloc) },
    }
}

fn draw_response<'a>(tape: &Tape, alloc: Alloc<'a>) -> Response<'a> {
    match tape.int_in(0u8..=8, "reply") {
        0 => Response::World { world: draw_world(tape) },
        1 => Response::Refused { refusal: draw_refusal(tape, alloc) },
        2 => Response::Text { path: draw_path(tape, alloc), start: draw_int(tape, "start"), text: draw_text(tape, "text", alloc) },
        3 => Response::Listing {
            entries: draw_list(tape, "entries", alloc, |t| Entry {
                name: draw_text(t, "name", alloc),
                kind: draw_code(t, EntryKind::ALL, "kind"),
            }),
        },
        4 => Response::Checked {
            answer: Answer {
                world: draw_world(tape),
                findings: draw_list(tape, "findings", alloc, |t| draw_finding(t, alloc)),
                certified: tape.coin("certified"),
                entered: Entered {
                    names: draw_int(tape, "names"),
                    heads: draw_int(tape, "heads"),
                    bodies: draw_int(tape, "bodies"),
                    lints: draw_int(tape, "lints"),
                },
                ms: draw_int(tape, "ms"),
            },
        },
        5 => Response::Changes {
            changes: draw_list(tape, "changes", alloc, |t| match t.int_in(0u8..=5, "change") {
                0 => Change::FileAdded { path: draw_path(t, alloc) },
                1 => Change::FileDeleted { path: draw_path(t, alloc) },
                2 => Change::FileChanged { path: draw_path(t, alloc) },
                3 => Change::ItemAdded { item: draw_item(t), path: draw_path(t, alloc) },
                4 => Change::ItemRemoved { item: draw_item(t), path: draw_path(t, alloc) },
                _ => Change::ItemChanged { item: draw_item(t), path: draw_path(t, alloc) },
            }),
        },
        6 => Response::Committed { files_written: draw_int(tape, "files written") },
        7 => Response::Moved { now: draw_world(tape) },
        _ => Response::Explained {
            why: draw_text(tape, "why", alloc),
            reads: draw_list(tape, "reads", alloc, |t| draw_text(t, "read", alloc)),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// The round trip.

/// decode(encode(r)) == r, its bytes written again equal, and its JSON rendering equal.
fn request_round_trip(tape: &Tape, alloc: Alloc<'_>) -> Result<(), Broke> {
    let request = draw_request(tape, alloc);
    let mut bytes = Vec::new_in(alloc);
    request.encode(&mut bytes);
    let back = Request::decode(&bytes, Limits::DEFAULT, alloc)
        .map_err(|error| Broke::Oracle(format!("{request:?} wrote {:02x?}, refused: {error}", bytes.as_slice())))?;
    if back != request {
        return Err(Broke::Oracle(format!("{request:?} came back as {back:?}")));
    }
    let mut again = Vec::new_in(alloc);
    back.encode(&mut again);
    if again.as_slice() != bytes.as_slice() {
        return Err(Broke::Oracle(format!("{request:?} written twice: {:02x?} then {:02x?}", bytes.as_slice(), again.as_slice())));
    }
    if back.to_value(alloc) != request.to_value(alloc) {
        return Err(Broke::Oracle(format!("{request:?}: two JSON renderings")));
    }
    Ok(())
}

fn response_round_trip(tape: &Tape, alloc: Alloc<'_>) -> Result<(), Broke> {
    let response = draw_response(tape, alloc);
    let mut bytes = Vec::new_in(alloc);
    response.encode(&mut bytes);
    let back = Response::decode(&bytes, Limits::DEFAULT, alloc)
        .map_err(|error| Broke::Oracle(format!("{response:?} wrote {:02x?}, refused: {error}", bytes.as_slice())))?;
    if back != response {
        return Err(Broke::Oracle(format!("{response:?} came back as {back:?}")));
    }
    let mut again = Vec::new_in(alloc);
    back.encode(&mut again);
    if again.as_slice() != bytes.as_slice() {
        return Err(Broke::Oracle(format!("{response:?} written twice: {:02x?} then {:02x?}", bytes.as_slice(), again.as_slice())));
    }
    Ok(())
}

#[test]
fn requests_round_trip() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    blaze_sim::check(Config::cases(400), |tape| request_round_trip(tape, alloc));
}

#[test]
fn responses_round_trip() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    blaze_sim::check(Config::cases(400), |tape| response_round_trip(tape, alloc));
}

// ---------------------------------------------------------------------------------------------
// Limits: one over is refused naming the field and the limit; exactly at it is accepted.

fn request_refused(bytes: &[u8], limits: Limits, alloc: Alloc<'_>) -> WireError {
    match Request::decode(bytes, limits, alloc) {
        Ok(request) => panic!("{bytes:02x?} decoded as {request:?}"),
        Err(error) => error,
    }
}

fn hex_refused(cbor: &str, alloc: Alloc<'_>) -> WireError {
    let bytes = hex(cbor, alloc);
    request_refused(&bytes, Limits::DEFAULT, alloc)
}

fn assert_refused(error: WireError, code: WireCode, field: &str, said: u64) {
    assert_eq!((error.code, error.field, error.said), (code, field, said), "{error}");
}

fn encoded<'a>(request: &Request<'_>, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut bytes = Vec::new_in(alloc);
    request.encode(&mut bytes);
    bytes
}

#[test]
fn every_limit_one_over_is_refused_by_name() {
    let frame = Frame::with_reserve(1usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let write = |p: &str, t: &str| Edit::WriteFile { path: path(p, alloc), text: text(t, alloc) };
    let patch = Request::Patch { base: world(0xaa), edits: list([write("abcd", "12345678"), write("b", ""), write("c", "")], alloc) };
    let bytes = encoded(&patch, alloc);
    let fits = Limits { most_bytes: bytes.len(), most_text: 8, most_path: 4, most_edits: 3, most_list: 3 };
    assert_eq!(
        Request::decode(&bytes, fits, alloc).as_ref(),
        Ok(&patch),
        "the patch at most_bytes {}, most_text 8, most_path 4, most_edits 3 exactly",
        bytes.len()
    );

    let error = request_refused(&bytes, Limits { most_bytes: bytes.len() - 1, ..fits }, alloc);
    assert_refused(error, WireCode::TooLarge, "message", crate::enc::wide(bytes.len() - 1));
    assert_refused(request_refused(&bytes, Limits { most_text: 7, ..fits }, alloc), WireCode::TooLong, "text", 7);
    assert_refused(request_refused(&bytes, Limits { most_path: 3, ..fits }, alloc), WireCode::TooLong, "path", 3);
    assert_refused(request_refused(&bytes, Limits { most_edits: 2, ..fits }, alloc), WireCode::TooMany, "edits", 2);

    let check = Request::Check {
        world: world(0xaa),
        select: list([Target::Item { key: item(1) }, Target::Item { key: item(2) }, Target::Item { key: item(3) }], alloc),
        lints: false,
    };
    let bytes = encoded(&check, alloc);
    // Its own message size: the check is longer than the patch above, whose size `fits` holds.
    let fits = Limits { most_bytes: bytes.len(), ..fits };
    match Request::decode(&bytes, fits, alloc) {
        Ok(decoded) => assert_eq!(decoded, check, "the check at most_list 3 exactly"),
        Err(error) => panic!("the check at most_list 3 and most_bytes {} exactly was refused: {error:?}", bytes.len()),
    }
    assert_refused(request_refused(&bytes, Limits { most_list: 2, ..fits }, alloc), WireCode::TooMany, "select", 2);
    let error = request_refused(&bytes, Limits { most_bytes: bytes.len() - 1, ..fits }, alloc);
    assert_refused(error, WireCode::TooLarge, "message", crate::enc::wide(bytes.len() - 1));
}

#[test]
fn each_encoding_rule_is_refused_by_name() {
    let frame = Frame::with_reserve(1usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    // The vector `request open`, a3 00 01 01 00 02 62 2f 77, is accepted.
    assert!(Request::decode(&hex("a3 00 01 01 00 02 62 2f 77", alloc), Limits::DEFAULT, alloc).is_ok());
    // An unknown version.
    assert_refused(hex_refused("a3 00 02 01 00 02 62 2f 77", alloc), WireCode::Version, "v", 2);
    // An unknown map key: key 9 after the open's three.
    assert_refused(hex_refused("a4 00 01 01 00 02 62 2f 77 09 00", alloc), WireCode::UnknownKey, "request", 9);
    // A duplicate key: key 0 twice.
    assert_refused(hex_refused("a3 00 01 00 01 02 62 2f 77", alloc), WireCode::DuplicateKey, "call", 0);
    // Keys out of order: key 0 after key 2.
    assert_refused(hex_refused("a4 00 01 01 00 02 62 2f 77 00 01", alloc), WireCode::KeyOrder, "request", 0);
    // A missing key: an open without its checkout.
    assert_refused(hex_refused("a2 00 01 01 00", alloc), WireCode::MissingKey, "checkout", 2);
    // A non-shortest integer: the version 1 as `18 01`.
    assert_refused(hex_refused("a3 00 18 01 01 00 02 62 2f 77", alloc), WireCode::NotShortest, "v", 0);
    // A non-shortest length: the checkout's two bytes said as `78 02`.
    assert_refused(hex_refused("a3 00 01 01 00 02 78 02 2f 77", alloc), WireCode::NotShortest, "checkout", 0);
    // An indefinite-length map, and an indefinite-length text.
    assert_refused(hex_refused("bf 00 01 01 00 02 62 2f 77 ff", alloc), WireCode::Indefinite, "call", 0);
    assert_refused(hex_refused("a3 00 01 01 00 02 7f 62 2f 77 ff", alloc), WireCode::Indefinite, "checkout", 0);
    // A call the schema does not have.
    assert_refused(hex_refused("a3 00 01 01 07 02 62 2f 77", alloc), WireCode::UnknownVariant, "call", 7);
    // A checkout that is not absolute; a path with a `..` part.
    assert_refused(hex_refused("a3 00 01 01 00 02 61 77", alloc), WireCode::BadPath, "checkout", 0);
    let dotdot = "a4 00 01 01 02 02 5820 aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa 03 a2 00 00 01 64 2e 2e 2f 78";
    assert_refused(hex_refused(dotdot, alloc), WireCode::BadPath, "path", 0);
    // An id of 31 bytes.
    let short = "a4 00 01 01 04 02 5820 aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa 03 581f aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa aaaaaaaaaaaaaa";
    assert_refused(hex_refused(short, alloc), WireCode::Range, "to", 0);
    // A range whose start is past its end: replace_range 5..2.
    let backwards = "a4 00 01 01 01 02 5820 aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaa 03 81 a6 00 03 01 61 61 02 05 03 02 04 60 05 60";
    assert_refused(hex_refused(backwards, alloc), WireCode::Range, "end", 2);
    // A float where the checkout's text is.
    assert_eq!(hex_refused("a3 00 01 01 00 02 f9 3c 00", alloc).code, WireCode::Cbor);
    // Bytes after the message.
    assert_refused(hex_refused("a3 00 01 01 00 02 62 2f 77 00", alloc), WireCode::Trailing, "message", 0);
    // An integer past 2^53 - 1: committed with files_written 2^53.
    let big = hex("a3 00 01 01 06 02 1b 00 20 00 00 00 00 00 00", alloc);
    match Response::decode(&big, Limits::DEFAULT, alloc) {
        Ok(response) => panic!("decoded as {response:?}"),
        Err(error) => assert_refused(error, WireCode::Range, "files_written", MOST_INT),
    }
    // A wire refusal sent back names the same code and field.
    let error = hex_refused("a3 00 02 01 00 02 62 2f 77", alloc);
    assert_eq!(
        error.refusal(alloc),
        Refusal::Wire { at: 2, what: WireCode::Version, field: text("v", alloc), said: 2 },
        "the version's value is at byte 2"
    );
}
