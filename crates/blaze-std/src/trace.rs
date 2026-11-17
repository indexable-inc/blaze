//! Spans and events (docs/v2/plan/design/std.md, "Nothing is global"): recorded through the
//! task's [`crate::Cx`] (`cx.span(..)`, `cx.event(..)`) into the recorder its `Io` owns, which
//! every context reaches from the root one. There is no global recorder.
//!
//! The recorder is bounded: past [`crate::io::Limits::trace_most`] records, said where the
//! `Io` is made, it counts what it drops rather than grow (rule bounded-progress).

use alloc::vec::Vec;
use core::time::Duration;

/// One thing a task said, on the driver's clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    pub at: Duration,
    /// The span it is in.
    pub span: &'static str,
    pub kind: RecordKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordKind {
    /// The span opened, inside `parent`.
    Opened { parent: &'static str },
    /// An event, named.
    Event(&'static str),
}

/// What a world's tasks recorded: from [`crate::Io::trace`].
#[derive(Debug)]
pub struct Trace {
    records: Vec<Record>,
    dropped: u64,
    /// The most records kept ([`crate::io::Limits::trace_most`]).
    most: usize,
}

impl Trace {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(most: usize) -> Trace {
        // alloc: the world's recorder, grown up to `most`.
        Trace { records: Vec::new(), dropped: 0, most }
    }

    #[blaze::pure]
    #[blaze::nonblocking]
    pub(crate) fn push(&mut self, record: Record) {
        if self.records.len() < self.most {
            self.records.push(record);
        } else {
            self.dropped += 1;
        }
    }

    /// The records kept, in the order they were made.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// How many records came after the recorder was full.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
