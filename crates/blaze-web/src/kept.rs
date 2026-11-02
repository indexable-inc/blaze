//! What the page's thread keeps of each workspace's records until a page is first asked for
//! (docs/web.md, "Where it comes from"): those since the build that said its functions whole,
//! and all of its last build's, to make the world from.
//!
//! A workspace's records are kept as runs, each a row of records of one build as they came. A
//! run's Began and Functions records (a cold build's are some megabytes) are one deflate
//! stream, written by the workspace's one encoder, whose state is reused from run to run; its
//! other records, which the next Began drops, are plain CBOR beside it. Each kept record is
//! its length (`u32`, little-endian) and its CBOR. What a Began or a whole object drops is
//! always a whole stream, so nothing is ever compressed twice.

#![forbid(unsafe_code)]

use std::io::{Read, Write};

use blaze_protocol::events::{Event, Record};
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};

/// How many workspaces' programs the daemon keeps.
pub(crate) const KEPT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Began,
    Functions { object: u32 },
    Other,
}

const _: () = assert!(size_of::<Kind>() == 8);

impl Kind {
    fn of(record: &Record) -> Kind {
        match &record.event {
            Event::Began { .. } => Kind::Began,
            Event::Functions { object, .. } => Kind::Functions { object: *object },
            _ => Kind::Other,
        }
    }
}

#[derive(Default)]
pub(crate) struct Kept {
    /// The workspace whose build began last at the end.
    workspaces: Vec<Workspace>,
    /// The first record that could not be kept: nothing is kept after it, and the world is
    /// refused rather than made from fewer records than came.
    broken: Option<Unkept>,
}

/// Why the records kept of a build cannot be given back whole.
#[derive(Debug)]
pub(crate) struct Unkept {
    pub(crate) root: u32,
    pub(crate) build: u64,
    pub(crate) why: Why,
}

#[derive(Debug)]
pub(crate) enum Why {
    Encode(blaze_protocol::cbor::Error),
    Deflate(std::io::Error),
    Inflate(std::io::Error),
    Decode(blaze_protocol::cbor::Error),
    /// The run's items end before its kinds do.
    EndsEarly,
}

impl std::fmt::Display for Unkept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Unkept { root, build, why } = self;
        write!(f, "build page: the records kept of build {build} of workspace {root} ")?;
        match why {
            Why::Encode(error) => write!(f, "do not encode: {error}"),
            Why::Deflate(error) => write!(f, "do not deflate: {error}"),
            Why::Inflate(error) => write!(f, "do not inflate: {error}"),
            Why::Decode(error) => write!(f, "do not decode: {error}"),
            Why::EndsEarly => f.write_str("end early"),
        }
    }
}

impl std::error::Error for Unkept {}

struct Workspace {
    root: u32,
    runs: Vec<Run>,
    /// The stream of the last run while it is written: `open` says it is.
    encoder: DeflateEncoder<Vec<u8>>,
    open: bool,
    /// The record being kept, encoded, and its encoder: reused from one record to the next.
    item: Vec<u8>,
    writer: blaze_protocol::cbor::Writer,
    /// Buffers kept for the next build, so that a build kept like the one before allocates
    /// nothing: runs dropped, cleared, for the next run; the encoder's other output, swapped
    /// in when a stream ends; the builds that said functions, as a Began reads them.
    spare: Vec<Run>,
    out: Vec<u8>,
    said: Vec<u64>,
}

/// The most bytes of output the encoder's spare keeps: a cold build's stream is not held twice
/// for good.
const SPARE_OUT: usize = 1 << 20;

/// Records of one build, in a row as they came.
struct Run {
    build: u64,
    /// Each record's kind, in order: a Began's or a Functions' is in `deflated`, any other's in
    /// `other`.
    kinds: Vec<Kind>,
    /// Empty while the run is open.
    deflated: Vec<u8>,
    other: Vec<u8>,
    /// The objects its Functions are of.
    objects: Vec<u32>,
}

impl Kept {
    /// Keeps `record`, of build `build` of the workspace numbered `root`.
    pub(crate) fn keep(&mut self, root: u32, build: u64, record: &Record) {
        if self.broken.is_some() {
            return;
        }
        let mut at = match self.workspaces.iter().position(|it| it.root == root) {
            Some(at) => at,
            None => {
                if self.workspaces.len() >= KEPT {
                    let oldest = (0..self.workspaces.len())
                        .min_by_key(|&it| self.workspaces[it].build());
                    if let Some(oldest) = oldest {
                        self.workspaces.remove(oldest);
                    }
                }
                self.workspaces.push(Workspace::new(root));
                self.workspaces.len() - 1
            }
        };
        if matches!(record.event, Event::Began { .. }) {
            let began = self.workspaces.remove(at);
            self.workspaces.push(began);
            at = self.workspaces.len() - 1;
        }
        if let Err(unkept) = self.workspaces[at].keep(build, record) {
            self.broken = Some(unkept);
        }
    }

    /// Hands each record kept to `apply`, in the order the world takes them: each workspace's
    /// in order, the workspace whose build began last last. Any record not given back whole
    /// is the error, before or after the ones it gave.
    pub(crate) fn drain(self, mut apply: impl FnMut(u32, u64, Record)) -> Result<(), Unkept> {
        if let Some(unkept) = self.broken {
            return Err(unkept);
        }
        // alloc: once, when the first page is asked for: every run inflates into it
        let mut inflated = Vec::new();
        for mut workspace in self.workspaces {
            workspace.close()?;
            let root = workspace.root;
            for run in &workspace.runs {
                inflated.clear();
                let build = run.build;
                DeflateDecoder::new(&run.deflated[..])
                    .read_to_end(&mut inflated)
                    .map_err(|error| Unkept { root, build, why: Why::Inflate(error) })?;
                let (mut kept, mut other) = (Items(&inflated), Items(&run.other));
                for kind in &run.kinds {
                    let item = if *kind == Kind::Other { other.next() } else { kept.next() };
                    let item = item.ok_or(Unkept { root, build, why: Why::EndsEarly })?;
                    let record = blaze_protocol::cbor::from_slice::<Record>(item)
                        .map_err(|error| Unkept { root, build, why: Why::Decode(error) })?;
                    apply(root, build, record);
                }
            }
        }
        Ok(())
    }

    /// Cuts the last byte off the last run's records of other kinds, for the tests.
    #[cfg(test)]
    pub(crate) fn tear(&mut self) {
        let run = self.workspaces.last_mut().and_then(|it| it.runs.last_mut());
        run.expect("a run kept").other.pop().expect("a record of another kind kept");
    }

    /// The build and kind of each record kept of the workspace `at` in order.
    #[cfg(test)]
    pub(crate) fn kinds(&self, at: usize) -> Vec<(u64, Kind)> {
        let Some(workspace) = self.workspaces.get(at) else { return Vec::new() };
        let runs = workspace.runs.iter();
        runs.flat_map(|run| run.kinds.iter().map(|&kind| (run.build, kind))).collect()
    }
}

/// How many dropped runs a workspace keeps for its next builds: a build makes one.
const SPARE_RUNS: usize = 2;

impl Run {
    fn new(build: u64) -> Run {
        // alloc: none until they fill; a run is made only when no spare is kept
        let (kinds, deflated, other, objects) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        Run { build, kinds, deflated, other, objects }
    }

    /// Empties it for another build, keeping its buffers but one too long to hold for good.
    fn clear(&mut self) {
        self.kinds.clear();
        self.objects.clear();
        for bytes in [&mut self.deflated, &mut self.other] {
            match bytes.capacity() > SPARE_OUT {
                // alloc: none, it frees the buffer
                true => *bytes = Vec::new(),
                false => bytes.clear(),
            }
        }
    }
}

impl Workspace {
    fn new(root: u32) -> Workspace {
        // alloc: once per workspace, the encoder's state; the buffers allocate as they fill
        let encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
        // alloc: none until they fill
        let (runs, spare, said) = (Vec::new(), Vec::new(), Vec::new());
        // alloc: none until they fill
        let (item, out) = (Vec::new(), Vec::new());
        let writer = blaze_protocol::cbor::Writer::new();
        Workspace { root, runs, encoder, open: false, item, writer, spare, out, said }
    }

    fn build(&self) -> u64 {
        self.runs.last().map_or(0, |it| it.build)
    }

    /// As [`crate::world::World::apply`] does: a Began ends the last build's records but for its
    /// functions, and the first object of a build said whole makes the program anew.
    fn keep(&mut self, build: u64, record: &Record) -> Result<(), Unkept> {
        let kind = Kind::of(record);
        match &record.event {
            Event::Began { .. } => {
                let mut said = std::mem::take(&mut self.said);
                said.clear();
                for run in &self.runs {
                    if !run.objects.is_empty() && said.last() != Some(&run.build) {
                        said.push(run.build);
                    }
                }
                // A run of a build that said no functions holds only its Began.
                self.retain(|run| {
                    run.other.clear();
                    run.kinds.retain(|it| *it != Kind::Other);
                    !run.kinds.is_empty() && said.contains(&run.build)
                });
                self.said = said;
            }
            Event::Functions { whole: true, object, .. } => {
                let said =
                    self.runs.iter().any(|it| it.build == build && it.objects.contains(object));
                if !said {
                    self.retain(|run| run.build == build);
                }
            }
            _ => {}
        }
        let mut item = std::mem::take(&mut self.item);
        item.clear();
        let kept = match self.writer.append(&mut item, record) {
            Ok(()) => self.keep_item(build, kind, &item),
            Err(error) => Err(Unkept { root: self.root, build, why: Why::Encode(error) }),
        };
        self.item = item;
        kept
    }

    fn keep_item(&mut self, build: u64, kind: Kind, item: &[u8]) -> Result<(), Unkept> {
        if !(self.open && self.runs.last().is_some_and(|it| it.build == build)) {
            self.close()?;
            let run = match self.spare.pop() {
                Some(mut run) => {
                    run.build = build;
                    run
                }
                None => Run::new(build),
            };
            self.runs.push(run);
            self.open = true;
        }
        let run = self.runs.last_mut().expect("a run was pushed if none was open");
        let len = (item.len() as u32).to_le_bytes();
        if kind == Kind::Other {
            run.other.extend_from_slice(&len);
            run.other.extend_from_slice(item);
        } else {
            self.encoder
                .write_all(&len)
                .and_then(|()| self.encoder.write_all(item))
                .map_err(|error| Unkept { root: self.root, build, why: Why::Deflate(error) })?;
        }
        if let Kind::Functions { object } = kind
            && !run.objects.contains(&object)
        {
            run.objects.push(object);
        }
        run.kinds.push(kind);
        Ok(())
    }

    /// Keeps the runs `keep` says to, in order; the stream of the last, if it goes, goes with
    /// it. Those dropped are kept spare, up to [`SPARE_RUNS`].
    fn retain(&mut self, mut keep: impl FnMut(&mut Run) -> bool) {
        let last = self.runs.len().wrapping_sub(1);
        let open = self.open;
        let (mut kept, mut dropped_open) = (0, false);
        for at in 0..self.runs.len() {
            if keep(&mut self.runs[at]) {
                // Every run before `kept` is kept, in order; the one at `kept` is dropped.
                self.runs.swap(kept, at);
                kept += 1;
            } else {
                dropped_open |= open && at == last;
            }
        }
        for mut run in self.runs.drain(kept..) {
            if self.spare.len() < SPARE_RUNS {
                run.clear();
                self.spare.push(run);
            }
        }
        if dropped_open {
            self.open = false;
            // What it held is dropped: the encoder only starts a stream of its own again.
            let out = std::mem::take(&mut self.out);
            if let Ok(stream) = self.encoder.reset(out) {
                self.spare_out(stream);
            }
        }
    }

    /// Ends the stream of the last run: its bytes are copied out whole, and the encoder's
    /// buffer kept for the next stream.
    fn close(&mut self) -> Result<(), Unkept> {
        if !std::mem::take(&mut self.open) {
            return Ok(());
        }
        let finished = self.encoder.reset(std::mem::take(&mut self.out));
        let run = self.runs.last_mut().expect("an open stream is the last run's");
        let (root, build) = (self.root, run.build);
        let stream = finished.map_err(|error| Unkept { root, build, why: Why::Deflate(error) })?;
        run.deflated.clear();
        run.deflated.reserve_exact(stream.len());
        run.deflated.extend_from_slice(&stream);
        self.spare_out(stream);
        Ok(())
    }

    /// Keeps `stream`'s buffer, emptied, for the encoder's next stream, but one too long to
    /// hold for good.
    fn spare_out(&mut self, mut stream: Vec<u8>) {
        if stream.capacity() <= SPARE_OUT {
            stream.clear();
            self.out = stream;
        }
    }
}

/// Items each a `u32` little-endian length and that many bytes.
struct Items<'a>(&'a [u8]);

impl<'a> Iterator for Items<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let (len, rest) = self.0.split_first_chunk::<4>()?;
        let len = u32::from_le_bytes(*len) as usize;
        if rest.len() < len {
            return None;
        }
        let (item, rest) = rest.split_at(len);
        self.0 = rest;
        Some(item)
    }
}

/// `records` as kept and drained, for the tests: the stream round-trips.
#[cfg(test)]
pub(crate) fn round_trip(
    records: &[(u32, u64, Record)],
) -> (Vec<(u64, Kind)>, Vec<(u32, u64, Record)>) {
    let mut kept = Kept::default();
    for (root, build, record) in records {
        kept.keep(*root, *build, record);
    }
    let kinds = kept.kinds(kept.workspaces.len().wrapping_sub(1));
    let mut out = Vec::new();
    kept.drain(|root, build, record| out.push((root, build, record))).expect("kept whole");
    (kinds, out)
}
