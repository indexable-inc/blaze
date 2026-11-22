//! The import: a machine over a [`CFrontEnd`] that answers its asks by probing each search
//! folder in turn (each probe a read of the list, present or absent), then writes the record,
//! seals the read list and keys both; and [`import`], the one function the expansion side calls.
//!
//! The expansion side's whole use (step 3):
//! 1. Parse the call ([`crate::grammar::parse`]) and compute its [`RequestKey`].
//! 2. Look the request key up in the store: a read list there whose [`ReadsView::first_change`]
//!    is `None` names the record under its [`ImportKey`]; no front end runs.
//!    (The store and that lookup are the expansion side's; this crate opens no store.)
//! 3. Otherwise call [`import`] with the pinned front end and the expansion's [`Files`] (its
//!    recorder of reads, which keeps the expansion's own read cells too), and store the record
//!    and the read list under their keys.
//! 4. [`crate::render::render`] the record for the request: Rust text for the expansion to lex,
//!    and the findings to report at the call.
//!
//! [`ReadsView::first_change`]: crate::key::ReadsView::first_change

use blaze_own::planted::NoPlant;
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::canon::encode;
use crate::finding::Finding;
use crate::front::{CFrontEnd, FileNo, FrontEffect, FrontError, FrontFault, FrontInput, FrontState};
use crate::grammar::Request;
use crate::key::{CDialect, CTarget, ImportKey, KeyInputs, ReadList, ReadsBuilder, ReadsError, RequestKey};
use crate::model::ModelError;
use crate::place::{Place, RelPath, Root};
use crate::render::{RenderError, render};
use crate::{at, narrow};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportState {
    /// A probe of one place is out ([`ImportEffect::Read`]).
    Probing,
    /// The front end is working with no ask out: step with [`ImportEvent::Tick`].
    Waiting,
    Done,
    Failed,
}

/// What the import asks its driver for: the bytes at one place, or that nothing is there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportEffect<'m> {
    Read { place: Place<'m> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportEvent<'e> {
    /// The probed place holds `text`.
    Present { text: &'e [u8] },
    /// Nothing is at the probed place.
    Absent,
    /// Time passed (for a front end that works between asks, and for deadlines).
    Tick,
}

/// Why an import stopped.
#[derive(Debug)]
pub enum ImportError<'a> {
    /// The front end stopped: in the header at `place` (`None`: before any, or the request as a
    /// whole), at byte `at`, for `fault`.
    Front { place: Option<(Root, AString<'a>)>, at: u32, fault: FrontFault },
    /// A place answered two ways in one import.
    Reads(ReadsError),
    /// The front end's model is not one the record takes.
    Model(ModelError),
    /// Stepped in a state that takes no event.
    OutOfState { state: ImportState },
    /// The driver's deadline for the front end passed (rule 18).
    Deadline,
    /// The rendering refused the record.
    Render(RenderError),
}

/// What an import is made from.
#[blaze::args]
pub struct ImportOpts<'r, F> {
    pub front: F,
    pub request: &'r Request<'r>,
    pub target: CTarget<'r>,
    pub dialect: CDialect,
    /// The digest of the toolchain the front end is.
    pub toolchain: blaze_ids::ContentId,
    pub now: u64,
    pub alloc: Alloc<'r>,
}

/// What an import makes: the declaration record and its read list (handed back together, rule
/// 76), and both keys.
pub struct Imported<'a> {
    pub request_key: RequestKey,
    pub key: ImportKey,
    pub record: AVec<'a, u8>,
    pub reads: ReadList<'a>,
}

/// The import machine (D170 C5).
pub struct Import<'a, F> {
    state: ImportState,
    front: Option<F>,
    request_key: RequestKey,
    reads: Option<ReadsBuilder<'a>>,
    /// The probe out: its place's text, its root, and which folder of the ask's search it is.
    probe: AString<'a>,
    probe_root: Root,
    dir: usize,
    /// The places of the files answered, by [`FileNo`]: their roots and text ranges in `opened`.
    opened: AString<'a>,
    opened_at: AVec<'a, (Root, u32, u32)>,
    done: Option<Imported<'a>>,
    alloc: Alloc<'a>,
}

impl<'a, F: CFrontEnd<'a>> Import<'a, F> {
    pub fn new(opts: ImportOpts<'a, F>) -> Result<Import<'a, F>, ImportError<'a>> {
        let request_key = RequestKey::of(KeyInputs {
            request: opts.request,
            target: opts.target,
            dialect: opts.dialect,
            toolchain: opts.toolchain,
        });
        let mut front = opts.front;
        let begun = front.step(opts.now, FrontInput::Begin { request: opts.request, target: opts.target, dialect: opts.dialect });
        let mut import = Import {
            state: ImportState::Waiting,
            front: Some(front),
            request_key,
            reads: Some(ReadsBuilder::new(opts.alloc)),
            probe: String::new_in(opts.alloc),
            probe_root: Root::Package,
            dir: 0,
            opened: String::new_in(opts.alloc),
            opened_at: Vec::new_in(opts.alloc),
            done: None,
            alloc: opts.alloc,
        };
        if let Err(e) = begun {
            return Err(import.front_error(e));
        }
        import.after_front(opts.now)?;
        Ok(import)
    }

    pub fn state(&self) -> ImportState {
        self.state
    }

    pub fn next_effect(&self) -> Option<ImportEffect<'_>> {
        match self.state {
            ImportState::Probing => {
                let path = RelPath::new(self.probe.as_str()).expect("a join of two RelPaths");
                Some(ImportEffect::Read { place: Place { root: self.probe_root, path } })
            }
            _ => None,
        }
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.front.as_ref().and_then(|f| f.next_deadline())
    }

    pub fn step(&mut self, now: u64, event: ImportEvent<'_>) -> Result<(), ImportError<'a>> {
        match (self.state, event) {
            (ImportState::Probing, ImportEvent::Present { text }) => {
                let path = RelPath::new(self.probe.as_str()).expect("a join of two RelPaths");
                let place = Place { root: self.probe_root, path };
                self.reads.as_mut().expect("kept until done").push(place, Some(text)).map_err(ImportError::Reads)?;
                let file = FileNo(narrow(self.opened_at.len()).ok_or(ImportError::Reads(ReadsError::TooMany))?);
                let start = narrow(self.opened.len()).ok_or(ImportError::Reads(ReadsError::TooMany))?;
                self.opened.push_str(path.as_str());
                let end = narrow(self.opened.len()).ok_or(ImportError::Reads(ReadsError::TooMany))?;
                self.opened_at.push((place.root, start, end));
                // The field, not `front_mut`: `place` borrows the probe's text beside it.
                let stepped = self.front.as_mut().expect("kept until done").step(now, FrontInput::File { file, place, text });
                self.stepped(now, stepped)
            }
            (ImportState::Probing, ImportEvent::Absent) => {
                let path = RelPath::new(self.probe.as_str()).expect("a join of two RelPaths");
                let place = Place { root: self.probe_root, path };
                self.reads.as_mut().expect("kept until done").push(place, None).map_err(ImportError::Reads)?;
                self.dir += 1;
                if self.probe_next() {
                    return Ok(());
                }
                let stepped = self.front_mut().step(now, FrontInput::Missing);
                self.stepped(now, stepped)
            }
            (ImportState::Waiting | ImportState::Probing, ImportEvent::Tick) => {
                if self.next_deadline().is_some_and(|d| now >= d) || self.state == ImportState::Waiting {
                    let stepped = self.front_mut().step(now, FrontInput::Tick);
                    return self.stepped(now, stepped);
                }
                Ok(())
            }
            (state, _) => {
                self.state = ImportState::Failed;
                Err(ImportError::OutOfState { state })
            }
        }
    }

    /// What the import made, once [`ImportState::Done`].
    pub fn finish(self) -> Result<Imported<'a>, ImportError<'a>> {
        match (self.state, self.done) {
            (ImportState::Done, Some(done)) => Ok(done),
            (state, _) => Err(ImportError::OutOfState { state }),
        }
    }

    fn front_mut(&mut self) -> &mut F {
        self.front.as_mut().expect("kept until done")
    }

    fn stepped(&mut self, now: u64, stepped: Result<(), FrontError>) -> Result<(), ImportError<'a>> {
        if let Err(e) = stepped {
            return Err(self.front_error(e));
        }
        self.after_front(now)
    }

    fn front_error(&mut self, e: FrontError) -> ImportError<'a> {
        self.state = ImportState::Failed;
        let place = e.file.and_then(|f| self.opened_at.get(at(f.0))).map(|&(root, start, end)| {
            let mut text = String::new_in(self.alloc);
            text.push_str(&self.opened.as_str()[at(start)..at(end)]);
            (root, text)
        });
        ImportError::Front { place, at: e.at, fault: e.fault }
    }

    /// Starts the probe of folder `self.dir` of the front end's ask; `false` when the search has
    /// no such folder.
    fn probe_next(&mut self) -> bool {
        let front = self.front.as_ref().expect("kept until done");
        let Some(FrontEffect::Read { name, search }) = front.next_effect() else {
            return false;
        };
        let Some(dir) = search.get(self.dir) else {
            return false;
        };
        self.probe = dir.join(name, self.alloc);
        self.probe_root = dir.root;
        self.state = ImportState::Probing;
        true
    }

    /// After the front end was stepped: probe its new ask, wait for it, or finish.
    fn after_front(&mut self, now: u64) -> Result<(), ImportError<'a>> {
        let state = self.front.as_ref().expect("kept until done").state();
        match state {
            FrontState::Reading => {
                self.dir = 0;
                if self.probe_next() {
                    return Ok(());
                }
                // An ask with no folder to search: answered at once, and it is no read.
                let stepped = self.front_mut().step(now, FrontInput::Missing);
                self.stepped(now, stepped)
            }
            FrontState::Working | FrontState::Begin => {
                self.state = ImportState::Waiting;
                Ok(())
            }
            FrontState::Failed => {
                self.state = ImportState::Failed;
                Err(ImportError::Front { place: None, at: 0, fault: FrontFault::OutOfState { state } })
            }
            FrontState::Done => {
                let front = self.front.take().expect("kept until done");
                let decls = match front.finish() {
                    Ok(decls) => decls,
                    Err(e) => return Err(self.front_error(e)),
                };
                let record = encode(&decls, self.alloc, NoPlant).map_err(|e| {
                    self.state = ImportState::Failed;
                    ImportError::Model(e)
                })?;
                let reads = self.reads.take().expect("kept until done").seal().map_err(|e| {
                    self.state = ImportState::Failed;
                    ImportError::Reads(e)
                })?;
                let key = ImportKey::of(self.request_key, &reads);
                self.done = Some(Imported { request_key: self.request_key, key, record, reads });
                self.state = ImportState::Done;
                Ok(())
            }
        }
    }
}

/// The driver's side of an import: the bytes at a place (`None` for nothing there), and the
/// time. The expansion's implementation is its recorder of reads over the package and the
/// toolchain's shelf; a test's is a map.
pub trait Files {
    fn read(&mut self, place: Place<'_>) -> Option<&[u8]>;
    fn now(&mut self) -> u64;
}

/// An import rendered: what the expansion side takes.
pub struct Expansion<'a> {
    pub imported: Imported<'a>,
    /// The module, as Rust text for the expansion to lex.
    pub rust: AString<'a>,
    /// Every declaration not rendered, or not all of it: the front end's unexpressed rows and
    /// the rendering's own, in the record's order.
    pub findings: AVec<'a, Finding>,
}

/// The import of `request` by `front`, its reads answered by `files`, rendered: the one function
/// the expansion side calls when no stored record holds (see the module's text). `deadline` is
/// the latest `files.now()` the front end may run to.
pub fn import<'a, F: CFrontEnd<'a>>(
    opts: ImportOpts<'a, F>,
    files: &mut impl Files,
    deadline: u64,
) -> Result<Expansion<'a>, ImportError<'a>> {
    let (request, alloc) = (opts.request, opts.alloc);
    let mut machine = Import::new(opts)?;
    loop {
        let now = files.now();
        if now > deadline {
            return Err(ImportError::Deadline);
        }
        match machine.state() {
            ImportState::Done => break,
            ImportState::Failed => return Err(ImportError::OutOfState { state: ImportState::Failed }),
            ImportState::Waiting => machine.step(now, ImportEvent::Tick)?,
            ImportState::Probing => {
                let Some(ImportEffect::Read { place }) = machine.next_effect() else {
                    return Err(ImportError::OutOfState { state: ImportState::Probing });
                };
                // The answer borrows `files`, not the machine, so it can step it.
                match files.read(place) {
                    Some(bytes) => machine.step(now, ImportEvent::Present { text: bytes })?,
                    None => machine.step(now, ImportEvent::Absent)?,
                }
            }
        }
    }
    let imported = machine.finish()?;
    // No unit yet: the expansion side does not hand the import its site, so `UnitKey::of` has
    // no input to be computed from here; bodies and header-local variables are findings.
    let rendered = render(&imported.record, request, None, alloc).map_err(ImportError::Render)?;
    Ok(Expansion { imported, rust: rendered.rust, findings: rendered.findings })
}
