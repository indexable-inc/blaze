//! What an extension is handed: its [`Cx`], every capability it has.
//!
//! By default that is three things: compute (bounded by fuel and by a stack depth), an
//! [`Arena`] to allocate in (capped), and the diagnostics sink ([`Cx::report`]); with them, the
//! facts its declaration asks for ([`Cx::fact`]). There is no file, socket, clock, environment
//! or process here, and none in `core`, which is all else an extension can name.

use std::{
    alloc::{AllocError, Allocator, Layout},
    fmt,
    ptr::NonNull,
};

use blaze_mem::Frame;

use crate::{
    Applicability, Fact, Item,
    host::{Finding, Fix, Job, Lookup, Stop, stop},
    mir::MirSpan,
};

/// The memory of one job of an extension: a bump allocator over the job's frame that stops
/// the job, naming it, when what it was asked for passes its cap. Freed whole when the job
/// ends; nothing in it outlives the job.
pub struct Arena<'f> {
    frame: &'f Frame,
    cap: usize,
    asked: std::cell::Cell<usize>,
}

impl<'f> Arena<'f> {
    pub(crate) fn new(frame: &'f Frame, cap: usize) -> Arena<'f> {
        Arena { frame, cap, asked: std::cell::Cell::new(0) }
    }

    /// The bytes asked of it so far. A bump allocator gives nothing back, so a vector that
    /// grew counts every size it had.
    pub fn asked(&self) -> usize {
        self.asked.get()
    }

    /// Counts `bytes` more, or stops the job at the cap.
    fn count(&self, bytes: usize) {
        let asked = self.asked.get().saturating_add(bytes);
        if asked > self.cap {
            stop(Stop::Memory { asked, cap: self.cap });
        }
        self.asked.set(asked);
    }
}

// SAFETY: every block comes from, and goes back to, `Frame`'s own `Allocator`, whose contract
// this forwards unchanged; the only thing added is a count taken before the frame is asked,
// and a stop (an unwind, before any memory changes hands) when the count passes the cap.
unsafe impl Allocator for Arena<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.count(layout.size());
        <Frame as Allocator>::allocate(self.frame, layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: `ptr` and `layout` are a block `allocate` above gave, so one the frame gave.
        unsafe { <Frame as Allocator>::deallocate(self.frame, ptr, layout) }
    }
}

/// A vector in a job's arena: the one growable collection an extension has. Made by
/// [`Cx::vec`]; it has no `new` of its own, since that would be the global heap.
pub type Vec<'a, T> = std::vec::Vec<T, &'a Arena<'a>>;

/// The bytes of a fact, in the job's arena: what [`Fact::encode`] writes.
pub type Bytes<'a> = Vec<'a, u8>;

/// Every capability of one job of an extension.
pub struct Cx<'a> {
    job: &'a Job<'a>,
    /// Which lint of the extension is running, for its findings; `None` in a fact's job.
    lint: Option<u32>,
}

impl<'a> Cx<'a> {
    pub(crate) fn new(job: &'a Job<'a>, lint: Option<u32>) -> Cx<'a> {
        Cx { job, lint }
    }

    /// The job's arena: `Vec::new_in(cx.arena())`, or [`Cx::vec`].
    pub fn arena(&self) -> &'a Arena<'a> {
        self.job.arena()
    }

    /// An empty vector in the job's arena.
    pub fn vec<T>(&self) -> Vec<'a, T> {
        std::vec::Vec::new_in(self.job.arena())
    }

    /// Reports a finding of the running lint at `at`, a span of the body's MIR (a
    /// terminator's, a statement's), which blaze places in the source after the job. What the
    /// finding says is formatted into text blaze owns, capped like the job's memory; the
    /// result adds notes, help and fixes.
    pub fn report(&mut self, at: MirSpan, message: fmt::Arguments<'_>) -> Report<'_, 'a> {
        self.job.charge(1);
        let Some(lint) = self.lint else {
            stop(Stop::Misuse { what: "a fact's function reported a finding: only a lint may" })
        };
        let message = self.job.text(message);
        let at = self.job.push(Finding {
            lint,
            at,
            message,
            notes: std::vec::Vec::new(), // alloc: a finding's parts, capped by the job's text
            helps: std::vec::Vec::new(), // alloc: as above
            fixes: std::vec::Vec::new(), // alloc: as above
        });
        Report { job: self.job, at }
    }

    /// The fact `F` of `item`: the body's own item or one it calls, of a fact the running lint
    /// or fact declared in its `requires` (a fact may also ask for itself, of its callees).
    /// `None` when blaze holds no such fact of the item: it is of another crate, or has no
    /// body. Inside a cycle of calls an item not computed yet answers [`Fact::bottom`]. An
    /// item on which the fact's own job failed stops this job too.
    /// Asking for a fact not declared, or of an item the body does not call, stops the job
    /// naming both: what a job may read is what its key covers.
    pub fn fact<F: Fact>(&self, item: Item) -> Option<F> {
        self.job.charge(1);
        match self.job.lookup(F::NAME, item) {
            Lookup::Value(bytes) => match F::decode(bytes) {
                Some(fact) => Some(fact),
                // alloc: a stop's text, once
                None => stop(Stop::Malformed { fact: F::NAME.to_owned() }),
            },
            Lookup::Unknown => None,
            // alloc: a stop's text, once
            Lookup::Undeclared => stop(Stop::Undeclared { fact: F::NAME.to_owned() }),
            // alloc: a stop's text, once
            Lookup::Failed => stop(Stop::Failed { fact: F::NAME.to_owned() }),
        }
    }
}

/// A finding being reported: what [`Cx::report`] gives back, to say more of it.
pub struct Report<'r, 'a> {
    job: &'r Job<'a>,
    at: usize,
}

impl Report<'_, '_> {
    /// Adds a note: why it is a finding.
    pub fn note(self, text: fmt::Arguments<'_>) -> Self {
        let text = self.job.text(text);
        self.job.amend(self.at, |it| it.notes.push(text));
        self
    }

    /// Adds help: what to do about it.
    pub fn help(self, text: fmt::Arguments<'_>) -> Self {
        let text = self.job.text(text);
        self.job.amend(self.at, |it| it.helps.push(text));
        self
    }

    /// Adds a fix: replace what is written at `at` (a span of the body's MIR) with
    /// `replacement`. `Applicability::MachineApplicable` is one `blaze check --fix` applies.
    pub fn fix(
        self,
        at: MirSpan,
        message: fmt::Arguments<'_>,
        replacement: fmt::Arguments<'_>,
        applicability: Applicability,
    ) -> Self {
        let message = self.job.text(message);
        let replacement = self.job.text(replacement);
        self.job
            .amend(self.at, |it| it.fixes.push(Fix { at, message, replacement, applicability }));
        self
    }
}
