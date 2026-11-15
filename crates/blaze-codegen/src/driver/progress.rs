//! How far a walk is, said while it runs: how many functions and statics it sent to be
//! described (`known`) and how many of those visits ended (`done`), recorded on the walk's
//! span every [`STRIDE`] visits and as each pass ends. A walk records its other counts only
//! as it ends, so one that ran for minutes said nothing of itself until then: tempo's cold
//! build (2026-11-12) showed `codegen walk (5 min 13 s)` for its last unit and no count.
//!
//! `known` only grows while describes reach more, so `done / known` is a floor of how much is
//! left, never an estimate of the end. The counts are of this run's schedule only while the
//! walk runs; as it ends they are the walk's (every item sent was visited), the same on any
//! pool.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};

/// Visits between two records of the counts on the span: one record per this many describes
/// (each 50 µs to seconds), so the cost is no part of a describe's.
const STRIDE: u64 = 1024;

/// A walk's counts and the span that carries them.
pub(super) struct Progress {
    done: AtomicU64,
    known: AtomicU64,
    /// The span open where the walk began (`codegen walk`), which has the fields `done` and
    /// `known`; a span without them (a test's walk under none) takes no record.
    span: tracing::Span,
}

impl Progress {
    /// The counts of a walk that begins here, carried by the span open on this thread.
    pub(super) fn here() -> Progress {
        Progress {
            done: AtomicU64::new(0),
            known: AtomicU64::new(0),
            span: tracing::Span::current(),
        }
    }

    /// `more` functions and statics were sent to be described.
    pub(super) fn known(&self, more: usize) {
        self.known.fetch_add(more as u64, Ordering::Relaxed);
    }

    /// One visit ended, on whichever thread.
    pub(super) fn done(&self) {
        let done = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        if done % STRIDE == 0 {
            self.tell();
        }
    }

    /// Records the counts on the span, as they are now.
    pub(super) fn tell(&self) {
        let (done, known) = self.counts();
        self.span.record("done", done);
        self.span.record("known", known);
    }

    /// Visits ended, and items sent.
    pub(super) fn counts(&self) -> (u64, u64) {
        (self.done.load(Ordering::Relaxed), self.known.load(Ordering::Relaxed))
    }
}
