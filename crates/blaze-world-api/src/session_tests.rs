//! The session of one connection: split delivery at every byte boundary gives the same request;
//! a second request, a message cut short, one nested too deep and one too large are refused by
//! name; one response a connection, and only after a request or a refusal.

use blaze_mem::{Frame, Tag};
use blaze_own::{Alloc, Erased, Vec};

use crate::{
    Edit, Limits, Refusal, Request, Response, Session, SessionError, SessionFeed, WireCode,
    test_support::{item, list, path, text, world},
};

fn every_edit<'a>(alloc: Alloc<'a>) -> Request<'a> {
    Request::Patch {
        base: world(0xaa),
        edits: list(
            [
                Edit::WriteFile { path: path("a.rs", alloc), text: text("x", alloc) },
                Edit::DeleteFile { path: path("b.rs", alloc) },
                Edit::RenameFile { from: path("c.rs", alloc), to: path("d.rs", alloc) },
                Edit::ReplaceRange {
                    path: path("e.rs", alloc),
                    start: 2,
                    end: 5,
                    text: text("yz", alloc),
                    expect: text("abc", alloc),
                },
                Edit::ReplaceItem { item: item(0xcc), text: text("fn f() {}", alloc) },
            ],
            alloc,
        ),
    }
}

fn wire(feed: Result<Option<Request<'_>>, SessionFeed>) -> crate::WireError {
    match feed {
        Err(SessionFeed::Wire(error)) => error,
        other => panic!("not a wire refusal: {other:?}"),
    }
}

#[test]
fn split_delivery_at_every_byte_gives_the_same_request() {
    let frame = Frame::with_reserve(1usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let request = every_edit(alloc);
    let mut bytes = Vec::new_in(alloc);
    request.encode(&mut bytes);
    for cut in 0..=bytes.len() {
        let mut session = Session::new(Limits::DEFAULT, alloc);
        let first = session.feed(&bytes[..cut]);
        match cut == bytes.len() {
            true => assert_eq!(first, Ok(Some(request.clone())), "whole at once"),
            false => {
                assert_eq!(first, Ok(None), "cut at {cut}: the first part is not whole");
                assert_eq!(session.feed(&bytes[cut..]), Ok(Some(request.clone())), "cut at {cut}");
            }
        }
    }
    // One byte a feed.
    let mut session = Session::new(Limits::DEFAULT, alloc);
    for (i, byte) in bytes.iter().enumerate() {
        let got = session.feed(core::slice::from_ref(byte));
        match i + 1 == bytes.len() {
            true => assert_eq!(got, Ok(Some(request.clone()))),
            false => assert_eq!(got, Ok(None), "byte {i}"),
        }
    }
}

#[test]
fn a_second_request_is_refused_by_name() {
    let frame = Frame::with_reserve(1usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let mut bytes = Vec::new_in(alloc);
    every_edit(alloc).encode(&mut bytes);
    let len = bytes.len();

    // In the same feed.
    let mut twice = Vec::new_in(alloc);
    twice.extend_from_slice(&bytes);
    twice.extend_from_slice(&bytes);
    let mut session = Session::new(Limits::DEFAULT, alloc);
    let error = wire(session.feed(&twice));
    assert_eq!((error.code, error.at), (WireCode::SecondRequest, len));

    // In a later feed, after the request was handed out.
    let mut session = Session::new(Limits::DEFAULT, alloc);
    assert!(matches!(session.feed(&bytes), Ok(Some(_))));
    assert_eq!(wire(session.feed(&[0xa0])).code, WireCode::SecondRequest);
    // The owed answer is then the refusal, once.
    let mut out = Vec::new_in(alloc);
    let refusal = Response::Refused { refusal: error.refusal(alloc) };
    assert_eq!(session.answer(&refusal, &mut out), Ok(()));
    assert_eq!(session.answer(&refusal, &mut out), Err(SessionError::Answered));
    assert_eq!(session.feed(&bytes), Err(SessionFeed::Session(SessionError::Closed)));
}

#[test]
fn one_answer_and_only_after_a_request_or_a_refusal() {
    let frame = Frame::with_reserve(1usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let mut bytes = Vec::new_in(alloc);
    every_edit(alloc).encode(&mut bytes);
    let answer = Response::Committed { files_written: 2 };
    let mut session = Session::new(Limits::DEFAULT, alloc);
    let mut out = Vec::new_in(alloc);
    assert_eq!(session.answer(&answer, &mut out), Err(SessionError::NotAsked));
    assert_eq!(session.feed(&bytes[..3]), Ok(None));
    assert_eq!(session.answer(&answer, &mut out), Err(SessionError::NotAsked));
    assert!(matches!(session.feed(&bytes[3..]), Ok(Some(_))));
    assert_eq!(session.answer(&answer, &mut out), Ok(()));
    // The vector `response committed`.
    assert_eq!(out.as_slice(), &[0xa3, 0x00, 0x01, 0x01, 0x06, 0x02, 0x02]);
    assert_eq!(session.answer(&answer, &mut out), Err(SessionError::Answered));
}

#[test]
fn a_message_cut_short_too_deep_or_too_large_is_refused_by_name() {
    let frame = Frame::with_reserve(1usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let mut bytes = Vec::new_in(alloc);
    every_edit(alloc).encode(&mut bytes);

    // The peer closes before the message is whole.
    let mut session = Session::new(Limits::DEFAULT, alloc);
    assert_eq!(session.feed(&bytes[..bytes.len() - 1]), Ok(None));
    match session.closed() {
        Err(SessionFeed::Wire(error)) => assert_eq!((error.code, error.at), (WireCode::Short, bytes.len() - 1)),
        other => panic!("{other:?}"),
    }
    // A refusal is answerable.
    let mut out = Vec::new_in(alloc);
    let refusal = Response::Refused {
        refusal: Refusal::Wire { at: 0, what: WireCode::Short, field: text("message", alloc), said: 0 },
    };
    assert_eq!(session.answer(&refusal, &mut out), Ok(()));

    // Arrays nested past the schema's depth, refused as they come, before the message is whole.
    let mut session = Session::new(Limits::DEFAULT, alloc);
    let error = wire(session.feed(&[0x81; 12]));
    assert_eq!(error.code, WireCode::TooDeep);
    assert_eq!(error.said, crate::enc::wide(crate::MOST_DEPTH));

    // A text that says it is longer than a message may be, refused at its head.
    let small = Limits { most_bytes: 64, ..Limits::DEFAULT };
    let mut session = Session::new(small, alloc);
    let error = wire(session.feed(&[0xa3, 0x00, 0x01, 0x01, 0x00, 0x02, 0x79, 0x03, 0xe8]));
    assert_eq!((error.code, error.said), (WireCode::TooLarge, 64));

    // More bytes than a message may be.
    let mut session = Session::new(Limits { most_bytes: 8, ..Limits::DEFAULT }, alloc);
    assert_eq!(wire(session.feed(&bytes)).code, WireCode::TooLarge);

    // A tag, which the schema has none of.
    let mut session = Session::new(Limits::DEFAULT, alloc);
    assert_eq!(wire(session.feed(&[0xc0, 0x00])).code, WireCode::Cbor);
}
