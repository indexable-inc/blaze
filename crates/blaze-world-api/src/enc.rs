//! Encode: every value to its one spelling (rule 25), through blaze-cbor's head writers.

use core::alloc::Allocator;

use blaze_cbor::write as w;
use blaze_own::Vec;

use crate::{
    Answer, Change, Edit, Entered, Entry, Finding, Note, Place, Refusal, Request, Response, Spot, Subject, Target, VERSION, What,
};

pub(crate) fn wide(n: usize) -> u64 {
    match u64::try_from(n) {
        Ok(n) => n,
        Err(_) => unreachable!("blaze's targets have a `usize` of 64 bits or fewer"),
    }
}

fn num<A: Allocator>(out: &mut Vec<u8, A>, key: u64, n: u64) {
    w::uint(out, key);
    w::uint(out, n);
}

fn id<A: Allocator>(out: &mut Vec<u8, A>, key: u64, bytes: &[u8; 32]) {
    w::uint(out, key);
    w::bytes(out, bytes);
}

fn txt<A: Allocator>(out: &mut Vec<u8, A>, key: u64, text: &str) {
    w::uint(out, key);
    w::text(out, text);
}

fn flag<A: Allocator>(out: &mut Vec<u8, A>, key: u64, value: bool) {
    w::uint(out, key);
    w::boolean(out, value);
}

fn list<A: Allocator, T>(out: &mut Vec<u8, A>, key: u64, items: &[T], mut each: impl FnMut(&mut Vec<u8, A>, &T)) {
    w::uint(out, key);
    w::array(out, wide(items.len()));
    for item in items {
        each(out, item);
    }
}

/// A map's head and its key 0: a variant's tag, or (with `version`) a message's version
/// then its call or reply at key 1.
fn tagged<A: Allocator>(out: &mut Vec<u8, A>, entries: u64, tag: u64) {
    w::map(out, entries);
    num(out, 0, tag);
}

impl Request<'_> {
    /// The request's bytes, appended to `out`.
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        let fields = match self {
            Request::Open { .. } => 1,
            Request::Patch { .. } | Request::Read { .. } | Request::Diff { .. } | Request::Explain { .. } => 2,
            Request::Check { .. } | Request::Commit { .. } => 3,
        };
        w::map(out, 2 + fields);
        num(out, 0, VERSION);
        num(out, 1, self.call().code());
        match self {
            Request::Open { checkout } => txt(out, 2, checkout.as_str()),
            Request::Patch { base, edits } => {
                id(out, 2, &base.0);
                list(out, 3, edits, |out, edit| edit.encode(out));
            }
            Request::Read { world, what } => {
                id(out, 2, &world.0);
                w::uint(out, 3);
                what.encode(out);
            }
            Request::Check { world, select, lints } => {
                id(out, 2, &world.0);
                list(out, 3, select, |out, target| target.encode(out));
                flag(out, 4, *lints);
            }
            Request::Diff { from, to } => {
                id(out, 2, &from.0);
                id(out, 3, &to.0);
            }
            Request::Commit { world, onto, expect } => {
                id(out, 2, &world.0);
                txt(out, 3, onto.as_str());
                id(out, 4, &expect.0);
            }
            Request::Explain { world, subject } => {
                id(out, 2, &world.0);
                w::uint(out, 3);
                subject.encode(out);
            }
        }
    }
}

impl Edit<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        match self {
            Edit::WriteFile { path, text } => {
                tagged(out, 3, 0);
                txt(out, 1, path.as_str());
                txt(out, 2, text.as_str());
            }
            Edit::DeleteFile { path } => {
                tagged(out, 2, 1);
                txt(out, 1, path.as_str());
            }
            Edit::RenameFile { from, to } => {
                tagged(out, 3, 2);
                txt(out, 1, from.as_str());
                txt(out, 2, to.as_str());
            }
            Edit::ReplaceRange { path, start, end, text, expect } => {
                tagged(out, 6, 3);
                txt(out, 1, path.as_str());
                num(out, 2, *start);
                num(out, 3, *end);
                txt(out, 4, text.as_str());
                txt(out, 5, expect.as_str());
            }
            Edit::ReplaceItem { item, text } => {
                tagged(out, 3, 4);
                id(out, 1, &item.0.0);
                txt(out, 2, text.as_str());
            }
        }
    }
}

impl What<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        match self {
            What::File { path } => {
                tagged(out, 2, 0);
                txt(out, 1, path.as_str());
            }
            What::Item { key } => {
                tagged(out, 2, 1);
                id(out, 1, &key.0.0);
            }
            What::Dir { path } => {
                tagged(out, 2, 2);
                txt(out, 1, path.as_ref().map_or("", |path| path.as_str()));
            }
        }
    }
}

impl Target<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        match self {
            Target::Path { path } => {
                tagged(out, 2, 0);
                txt(out, 1, path.as_str());
            }
            Target::Item { key } => {
                tagged(out, 2, 1);
                id(out, 1, &key.0.0);
            }
        }
    }
}

impl Subject {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        match self {
            Subject::Finding { id: finding } => {
                tagged(out, 2, 0);
                id(out, 1, &finding.0);
            }
            Subject::Item { key } => {
                tagged(out, 2, 1);
                id(out, 1, &key.0.0);
            }
        }
    }
}

impl Response<'_> {
    /// Its number on the wire: key 1 of a response.
    pub fn reply_code(&self) -> u64 {
        match self {
            Response::World { .. } => 0,
            Response::Refused { .. } => 1,
            Response::Text { .. } => 2,
            Response::Listing { .. } => 3,
            Response::Checked { .. } => 4,
            Response::Changes { .. } => 5,
            Response::Committed { .. } => 6,
            Response::Moved { .. } => 7,
            Response::Explained { .. } => 8,
        }
    }

    /// The response's bytes, appended to `out`.
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        let fields = match self {
            Response::World { .. }
            | Response::Refused { .. }
            | Response::Listing { .. }
            | Response::Changes { .. }
            | Response::Committed { .. }
            | Response::Moved { .. } => 1,
            Response::Explained { .. } => 2,
            Response::Text { .. } => 3,
            Response::Checked { .. } => 5,
        };
        w::map(out, 2 + fields);
        num(out, 0, VERSION);
        num(out, 1, self.reply_code());
        match self {
            Response::World { world } => id(out, 2, &world.0),
            Response::Refused { refusal } => {
                w::uint(out, 2);
                refusal.encode(out);
            }
            Response::Text { path, start, text } => {
                txt(out, 2, path.as_str());
                num(out, 3, *start);
                txt(out, 4, text.as_str());
            }
            Response::Listing { entries } => list(out, 2, entries, |out, entry| entry.encode(out)),
            Response::Checked { answer } => answer.encode_fields(out),
            Response::Changes { changes } => list(out, 2, changes, |out, change| change.encode(out)),
            Response::Committed { files_written } => num(out, 2, *files_written),
            Response::Moved { now } => id(out, 2, &now.0),
            Response::Explained { why, reads } => {
                txt(out, 2, why.as_str());
                list(out, 3, reads, |out, read| w::text(out, read.as_str()));
            }
        }
    }
}

impl Answer<'_> {
    /// Keys 2 to 6 of a `checked` reply.
    fn encode_fields<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        id(out, 2, &self.world.0);
        list(out, 3, &self.findings, |out, finding| finding.encode(out));
        flag(out, 4, self.certified);
        w::uint(out, 5);
        self.entered.encode(out);
        num(out, 6, self.ms);
    }
}

impl Entered {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        w::map(out, 4);
        num(out, 0, self.names);
        num(out, 1, self.heads);
        num(out, 2, self.bodies);
        num(out, 3, self.lints);
    }
}

impl Entry<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        w::map(out, 2);
        txt(out, 0, self.name.as_str());
        num(out, 1, self.kind.code());
    }
}

impl Finding<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        w::map(out, 6);
        id(out, 0, &self.id.0);
        txt(out, 1, self.code.as_str());
        txt(out, 2, self.message.as_str());
        w::uint(out, 3);
        self.at.encode(out);
        list(out, 4, &self.notes, |out, note| note.encode(out));
        list(out, 5, &self.fix, |out, edit| edit.encode(out));
    }
}

impl Spot<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        w::map(out, 3);
        id(out, 0, &self.item.0.0);
        txt(out, 1, self.path.as_str());
        w::uint(out, 2);
        self.place.encode(out);
    }
}

impl Place {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        w::map(out, 4);
        num(out, 0, self.start);
        num(out, 1, self.end);
        num(out, 2, self.line);
        num(out, 3, self.column);
    }
}

impl Note<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        w::map(out, 2);
        txt(out, 0, self.message.as_str());
        list(out, 1, &self.at, |out, spot| spot.encode(out));
    }
}

impl Change<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        match self {
            Change::FileAdded { path } | Change::FileDeleted { path } | Change::FileChanged { path } => {
                let tag = match self {
                    Change::FileAdded { .. } => 0,
                    Change::FileDeleted { .. } => 1,
                    _ => 2,
                };
                tagged(out, 2, tag);
                txt(out, 1, path.as_str());
            }
            Change::ItemAdded { item, path } | Change::ItemRemoved { item, path } | Change::ItemChanged { item, path } => {
                let tag = match self {
                    Change::ItemAdded { .. } => 3,
                    Change::ItemRemoved { .. } => 4,
                    _ => 5,
                };
                tagged(out, 3, tag);
                id(out, 1, &item.0.0);
                txt(out, 2, path.as_str());
            }
        }
    }
}

impl Refusal<'_> {
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        match self {
            Refusal::Edit { index, why } => {
                tagged(out, 3, 0);
                num(out, 1, *index);
                num(out, 2, why.code());
            }
            Refusal::NoSuchWorld { world } => {
                tagged(out, 2, 1);
                id(out, 1, &world.0);
            }
            Refusal::NotACheckout { checkout } => {
                tagged(out, 2, 2);
                txt(out, 1, checkout.as_str());
            }
            Refusal::NoSuchPath { path } => {
                tagged(out, 2, 3);
                txt(out, 1, path.as_str());
            }
            Refusal::NoSuchItem { item } => {
                tagged(out, 2, 4);
                id(out, 1, &item.0.0);
            }
            Refusal::NoSuchFinding { id: finding } => {
                tagged(out, 2, 5);
                id(out, 1, &finding.0);
            }
            Refusal::Wire { at, what, field, said } => {
                tagged(out, 5, 6);
                num(out, 1, *at);
                num(out, 2, what.code());
                txt(out, 3, field.as_str());
                num(out, 4, *said);
            }
            Refusal::Io { path, message } => {
                tagged(out, 3, 7);
                txt(out, 1, path.as_str());
                txt(out, 2, message.as_str());
            }
            Refusal::Unsupported { call, why } => {
                tagged(out, 3, 8);
                num(out, 1, call.code());
                txt(out, 2, why.as_str());
            }
        }
    }
}
