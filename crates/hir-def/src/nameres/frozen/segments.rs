//! Segments: values written with [`Enc`] so that each is read alone with [`Dec`], for a
//! record another crate lays out (hir-ty's impl table pages; docs/v2/plan/design/restore.md,
//! "Def maps read in place").
//!
//! [`Enc::finish`] gives one stream, which a reader takes from its start. A record read a
//! part at a time is segments instead: each written whole (a macro call or a syntax context
//! is written in full the first time a *segment* names it), all naming their files by an
//! index into one table for the record. [`Writer`] writes them, and gives the body they are
//! in and the files table apart, so the record's owner places both. [`files`] reads the
//! table once a crate; [`reader`] reads one segment against it, borrowing the crate's
//! closure and modules from their memos, so a reader is made with no allocation.
//!
//! Nothing here knows a layout: where a segment is, and how it is found, is its record's.

use std::borrow::Cow;

use base_db::{Crate, Db, FileId};
use blaze_format::cursor::Reader;
use blaze_hash::HashMap;
use std::alloc::Global;

use super::{Dec, Enc, R, closure};
use crate::ModuleId;

/// Writes the segments of one record of values of a crate.
pub struct Writer<'a> {
    enc: Enc<'a>,
    /// Every segment so far, one after another.
    body: Vec<u8>,
}

/// What a [`Writer`] wrote: the files table every segment names its files by, and the body
/// the segments are in. A segment's place, as [`Writer::segment`] gave it, is from the
/// body's start.
pub struct Written {
    pub files: Vec<u8>,
    pub body: Vec<u8>,
}

impl<'a> Writer<'a> {
    /// A writer of segments of values of `krate`.
    pub fn new(db: &'a Db, krate: Crate) -> Self {
        // alloc: encoding, once per crate per write of its record (not on a build's path)
        Writer { enc: Enc::new(db, krate), body: Vec::new() }
    }

    /// The encoder, for what is not a segment: [`Enc::sorted`] and [`Enc::fingerprint`],
    /// which leave nothing written.
    pub fn enc(&mut self) -> &mut Enc<'a> {
        &mut self.enc
    }

    /// Writes one segment, and gives where it is in the body: its start and its length.
    pub fn segment(&mut self, write: impl FnOnce(&mut Enc<'a>) -> R) -> R<(u32, u32)> {
        // A segment reads without any other: what it names is written whole the first time
        // it names it, whatever the segments before it named.
        self.enc.calls.clear();
        self.enc.contexts.clear();
        self.enc.out.clear();
        write(&mut self.enc)?;
        let at = u32::try_from(self.body.len()).map_err(|_| "a record over 4 GiB")?;
        let len = u32::try_from(self.enc.out.len()).map_err(|_| "a segment over 4 GiB")?;
        self.body.extend_from_slice(&self.enc.out);
        self.enc.out.clear();
        Ok((at, len))
    }

    /// Adds `bytes` to the body as they are (a table of places, which names nothing), and
    /// gives where they start.
    pub fn raw(&mut self, bytes: &[u8]) -> R<u32> {
        let at = u32::try_from(self.body.len()).map_err(|_| "a record over 4 GiB")?;
        self.body.extend_from_slice(bytes);
        Ok(at)
    }

    /// The files table and the body. Refuses what [`Enc::finish`] refuses: bytes that name a
    /// place of one checkout.
    pub fn finish(mut self) -> R<Written> {
        self.enc.calls.clear();
        self.enc.contexts.clear();
        let body_len = self.body.len();
        self.enc.out = self.body;
        let mut files = self.enc.finish()?;
        let body = files.split_off(files.len() - body_len);
        Ok(Written { files, body })
    }
}

/// The files a record's segments name, read from its files table: each checked against the
/// content the record was made from, as [`Dec::new_checked`] checks them.
#[derive(Debug, PartialEq, Eq)]
pub struct Files(Box<[FileId]>);

/// Reads the files table `bytes` of a record of values of `krate` ([`Written::files`]).
pub fn files(db: &Db, krate: Crate, bytes: &[u8]) -> R<Files> {
    let mut dec = Dec::new(db, krate, bytes);
    dec.files()?;
    dec.finish()?;
    // alloc: the table's own storage, once per crate read from the record
    Ok(Files(dec.files.into_owned().into_boxed_slice()))
}

/// `krate` and the crates it depends on, as [`closure`] lists them: what every segment's
/// reader names crates by, made once a crate and not once a read.
#[blaze_query::tracked(returns(ref))]
fn segment_closure(db: &Db, krate: Crate) -> Vec<Crate> {
    closure(db, krate)
}

/// The modules of `krate` in its `DefMap`'s order: what a segment names the crate's own
/// modules by, made once a crate and not once a read.
#[blaze_query::tracked(returns(ref))]
fn segment_modules(db: &Db, krate: Crate) -> Box<[ModuleId]> {
    // alloc: the memo's value, once per crate read from a record of segments
    crate::nameres::crate_def_map(db, krate).table(db).keys().copied().collect()
}

/// A reader of the segment `bytes` of a record of values of `krate` whose files table read
/// as `files`.
pub fn reader<'a>(db: &'a Db, krate: Crate, files: &'a Files, bytes: &'a [u8]) -> Dec<'a> {
    Dec {
        db,
        r: Reader::new(bytes),
        krate,
        closure: Cow::Borrowed(segment_closure(db, krate).as_slice()),
        paths: HashMap::new_in(Global),
        files: Cow::Borrowed(&files.0),
        // alloc: none until the segment names a macro call
        calls: Vec::new(),
        // alloc: none until the segment names a syntax context
        contexts: Vec::new(),
        own: Cow::Borrowed(&**segment_modules(db, krate)),
        in_def_map: true,
        modules: HashMap::new_in(Global),
    }
}
