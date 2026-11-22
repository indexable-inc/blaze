//! Where the report goes: a [`Sink`], made by the launcher from the program's grant (its
//! spec's `telemetry` grant: nothing ambient, no environment variable), held by the runtime,
//! and written by the runtime once at exit on every exit path: `main` returned, failed or
//! panicked, and with any number of tasks failed by their own panic (rule 20). Never a key,
//! never a record (rule 25): the sink is the report's one way out.
//!
//! The grant's kind (`grant.rs` `Kind::Telemetry`, `impl Grant for Sink`) is a line for that
//! file's owner (the module doc of [`super`] lists it); `Sink::new` is what the grant calls.

use core::{alloc::Allocator, fmt};

use blaze_own::Vec;

use super::{Level, Report, TelemetryError};
use crate::{
    cx::Cx,
    fs::{Content, Dir, Durable, FsError, ReadWrite},
    out::{Out, OutError},
    path::Path,
};

/// The report's form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// The text rendering (`Report`'s `Display`).
    Text,
    /// The CBOR encoding (`Report::encode_into`).
    Cbor,
}

/// Where a sink writes.
#[derive(Clone)] // clone: a stream's or directory's authority, copied
pub enum SinkTo {
    /// Kept (`Totals` always is), written nowhere.
    Nowhere,
    /// Standard error (the `err` stream the launcher holds), as text.
    Err(Out),
    /// A file beneath a directory the program was granted, replaced whole at exit.
    File { dir: Dir<ReadWrite>, path: Path, form: Form },
}

/// How a sink is made.
#[derive(Clone)] // clone: as `SinkTo`
pub struct SinkOpts {
    pub to: SinkTo,
    /// What the run keeps: a choice, never defaulted here (D125).
    pub level: Level,
}

/// The run's sink.
#[derive(Clone)] // clone: as `SinkTo`
pub struct Sink {
    to: SinkTo,
    level: Level,
}

/// What a sink's write refused.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum SinkError {
    Report(TelemetryError),
    Out(OutError),
    File(FsError),
}

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SinkError::Report(error) => write!(f, "the telemetry report: {error}"),
            SinkError::Out(error) => write!(f, "the telemetry report to standard error: {error}"),
            SinkError::File(error) => write!(f, "the telemetry report's file: {error}"),
        }
    }
}

impl Sink {
    pub fn new(opts: SinkOpts) -> Sink {
        Sink { to: opts.to, level: opts.level }
    }

    /// The level the runtime makes every worker's cell at.
    pub fn level(&self) -> Level {
        self.level
    }

    /// Whether anything is written.
    pub fn writes(&self) -> bool {
        !matches!(self.to, SinkTo::Nowhere)
    }

    /// `report` written, its bytes made in `scratch` (the exit's frame). Refused for a report
    /// not finished.
    pub fn write<A: Allocator + Copy, B: Allocator + Copy>(&self, cx: &Cx, report: &Report<A>, scratch: B) -> Result<(), SinkError> {
        if !report.finished() {
            return Err(SinkError::Report(TelemetryError::Unfinished));
        }
        match &self.to {
            SinkTo::Nowhere => Ok(()),
            SinkTo::Err(out) => out.line(cx, format_args!("{report}")).map_err(SinkError::Out),
            SinkTo::File { dir, path, form } => {
                let mut bytes: Vec<u8, B> = Vec::new_in(scratch);
                match form {
                    Form::Cbor => report.encode_into(&mut bytes).map_err(SinkError::Report)?,
                    Form::Text => text_into(report, &mut bytes)?,
                }
                dir.publish(cx, path, Content::bytes(&bytes), Durable::No).map(|_| ()).map_err(SinkError::File)
            }
        }
    }
}

/// `report`'s text appended to `bytes`: what a sink writes as text, and what the runtime
/// writes to standard error at exit (where no context is left to hand an `Out`).
pub(crate) fn text_into<A: Allocator + Copy, B: Allocator>(report: &Report<A>, bytes: &mut Vec<u8, B>) -> Result<(), SinkError> {
    if !report.finished() {
        return Err(SinkError::Report(TelemetryError::Unfinished));
    }
    let mut text = Text { bytes };
    // A `fmt::Write` into the caller's allocator: its only error is a reservation refused.
    if fmt::write(&mut text, format_args!("{report}")).is_err() {
        return Err(SinkError::Report(TelemetryError::Reserve { what: "the report's text", bytes: 0 }));
    }
    Ok(())
}

/// A `fmt::Write` appending to bytes in the caller's allocator.
struct Text<'a, B: Allocator> {
    bytes: &'a mut Vec<u8, B>,
}

impl<B: Allocator> fmt::Write for Text<'_, B> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.bytes.try_reserve(s.len()).map_err(|_| fmt::Error)?;
        self.bytes.extend_from_slice(s.as_bytes());
        Ok(())
    }
}
