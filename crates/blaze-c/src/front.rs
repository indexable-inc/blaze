//! The seam (c.md part 2): a C front end is a machine (D170 C5) that reads a header only by
//! asking for it.
//!
//! It is given the request ([`FrontInput::Begin`]); it asks for each file it opens, the root
//! headers first, by [`FrontEffect::Read`]: the name as the source spells it and the folders to
//! search, in order; it is answered with the first folder's file that exists
//! ([`FrontInput::File`]) or with [`FrontInput::Missing`]; and when it is [`FrontState::Done`],
//! [`CFrontEnd::finish`] hands over the model. The import machine ([`crate::import::Import`])
//! does the search and keeps the read list (every place it probed, present or absent), so a
//! front end cannot read a file the list leaves out: its only way to a file's bytes is the
//! answer to its ask.
//!
//! The first front end is the pinned clang, run natively in its own process as blaze's own
//! hermetic tool (step 2); its driver turns clang's file opens into these asks. A second, blaze's
//! own declaration front end, can stand beside it and be held byte-equal by the differential
//! compare ([`crate::differ`]). [`ModelFrontEnd`] is the third, for tests: its declarations are
//! given as data, and it asks for the files a test names.

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::grammar::Request;
use crate::key::{CAbi, CDialect, CTarget, Endian};
use crate::model::{AbiRow, DeclsBuilder, ModelError};
use crate::place::{Dir, Place, PlaceError, RelPath, Root};
use crate::{at, narrow};

/// A file the front end was answered with, numbered from 0 in the order answered.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FileNo(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrontState {
    /// Waiting for [`FrontInput::Begin`].
    Begin,
    /// Has a [`FrontEffect::Read`] out.
    Reading,
    /// Working with no ask out: a driver steps it with [`FrontInput::Tick`] until it asks or
    /// is done (a front end in another process answers on its own time).
    Working,
    Done,
    Failed,
}

/// Folders to search, in order, as a view a front end hands out with its ask.
#[derive(Clone, Copy, Debug)]
pub struct Search<'m> {
    text: &'m str,
    dirs: &'m [SearchDir],
}

/// One folder of a [`Search`]: its root and its path's bytes in the search's text (an empty
/// range is the root itself).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SearchDir {
    pub root: Root,
    pub start: u32,
    pub end: u32,
}

impl<'m> Search<'m> {
    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    pub fn get(&self, k: usize) -> Option<Dir<'m>> {
        let d = self.dirs.get(k)?;
        let path = match d.start == d.end {
            true => None,
            false => Some(RelPath::new(&self.text[at(d.start)..at(d.end)]).expect("checked by SearchList::push")),
        };
        Some(Dir { root: d.root, path })
    }
}

/// An owned list of folders to search, which a front end keeps and lends out as a [`Search`].
pub struct SearchList<'a> {
    text: AString<'a>,
    dirs: AVec<'a, SearchDir>,
}

impl<'a> SearchList<'a> {
    pub fn new(alloc: Alloc<'a>) -> SearchList<'a> {
        SearchList { text: String::new_in(alloc), dirs: Vec::new_in(alloc) }
    }

    pub fn push(&mut self, dir: Dir<'_>) -> Result<(), PlaceError> {
        let start = narrow(self.text.len()).ok_or(PlaceError::TooLong)?;
        if let Some(path) = dir.path {
            self.text.push_str(path.as_str());
        }
        let end = narrow(self.text.len()).ok_or(PlaceError::TooLong)?;
        self.dirs.push(SearchDir { root: dir.root, start, end });
        Ok(())
    }

    pub fn as_search(&self) -> Search<'_> {
        Search { text: self.text.as_str(), dirs: self.dirs.as_slice() }
    }
}

/// What a front end asks for.
#[derive(Clone, Copy, Debug)]
pub enum FrontEffect<'m> {
    /// The first of `search`'s folders that holds `name`, and its text.
    Read { name: RelPath<'m>, search: Search<'m> },
}

/// What a front end is stepped with.
#[derive(Clone, Copy, Debug)]
pub enum FrontInput<'e> {
    /// The request, the target and the dialect: the front end copies what it keeps.
    Begin { request: &'e Request<'e>, target: CTarget<'e>, dialect: CDialect },
    /// The answer to the ask out: the file found, as the `file`-th answered.
    File { file: FileNo, place: Place<'e>, text: &'e [u8] },
    /// The answer to the ask out: no folder holds the name.
    Missing,
    /// Time passed, with no answer: for a front end that works between asks.
    Tick,
}

/// Why a front end stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrontFault {
    /// An `#include` (or a root header) no folder holds.
    NotFound,
    /// A C error, by the front end's diagnostic code.
    Diagnostic { code: u32 },
    /// A model the builder refused.
    Model(ModelError),
    /// A path of the request or of an `#include` that is not a relative path under its root.
    Path(PlaceError),
    /// Stepped with an input its state does not take.
    OutOfState { state: FrontState },
    /// The front end's process ended without an answer (a crash: rule 20 fails this task alone),
    /// with its exit status.
    Ended { status: i32 },
    /// Its deadline passed (rule 18).
    Deadline,
}

/// A front end's error: the file it is in (`None` before the first, or for the request as a
/// whole), the byte in that file, and the fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrontError {
    pub file: Option<FileNo>,
    pub at: u32,
    pub fault: FrontFault,
}

/// A C front end, a machine on D170 C5's shape.
pub trait CFrontEnd<'a> {
    fn step(&mut self, now: u64, input: FrontInput<'_>) -> Result<(), FrontError>;
    /// The ask out, if any: borrowed from the front end, unchanged until it is stepped.
    fn next_effect(&self) -> Option<FrontEffect<'_>>;
    /// When it must next be stepped (with [`FrontInput::Tick`]) whatever happens.
    fn next_deadline(&self) -> Option<u64>;
    fn state(&self) -> FrontState;
    /// The model, once [`FrontState::Done`].
    fn finish(self) -> Result<DeclsBuilder<'a>, FrontError>;
}

/// The target facts the record carries.
pub fn abi_row(abi: &CAbi) -> AbiRow {
    AbiRow {
        pointer_size: abi.pointer.size,
        pointer_align: abi.pointer.align,
        little_endian: u8::from(abi.endian == Endian::Little),
        char_signed: u8::from(abi.char_signed),
    }
}

/// An `#include` a [`ModelFrontEnd`] asks for after the root headers: the name as spelled, and
/// whether in `<>` (searched in the include folders, then the shelf) or `""` (the first root
/// header's folder first).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModelInclude<'t> {
    pub name: &'t str,
    pub angled: bool,
}

/// The front end for tests: it asks for each root header and then each of `includes`, and on
/// the last answer builds its model by `build`, which is the test's data. A missing file stops
/// it with [`FrontFault::NotFound`].
#[cfg(any(test, feature = "model-front-end"))]
pub struct ModelFrontEnd<'a, B> {
    state: FrontState,
    includes: &'a [ModelInclude<'a>],
    build: Option<B>,
    /// The root headers' paths, then `includes`' names, in `names`' text.
    names: SearchList<'a>,
    headers: usize,
    next: usize,
    /// The root headers' search (the package), the quoted and the angled includes' searches.
    package: SearchList<'a>,
    quoted: SearchList<'a>,
    angled: SearchList<'a>,
    decls: Option<DeclsBuilder<'a>>,
    alloc: Alloc<'a>,
}

#[cfg(any(test, feature = "model-front-end"))]
impl<'a, B: FnOnce(&mut DeclsBuilder<'a>) -> Result<(), ModelError>> ModelFrontEnd<'a, B> {
    pub fn new(includes: &'a [ModelInclude<'a>], build: B, alloc: Alloc<'a>) -> ModelFrontEnd<'a, B> {
        ModelFrontEnd {
            state: FrontState::Begin,
            includes,
            build: Some(build),
            names: SearchList::new(alloc),
            headers: 0,
            next: 0,
            package: SearchList::new(alloc),
            quoted: SearchList::new(alloc),
            angled: SearchList::new(alloc),
            decls: None,
            alloc,
        }
    }

    fn error(&self, at: u32, fault: FrontFault) -> FrontError {
        let file = match self.next {
            0 => None,
            n => narrow(n - 1).map(FileNo),
        };
        FrontError { file, at, fault }
    }

    fn total(&self) -> usize {
        self.headers + self.includes.len()
    }

    fn fail(&mut self, error: FrontError) -> Result<(), FrontError> {
        self.state = FrontState::Failed;
        Err(error)
    }

    /// The names to ask for and the three searches: the package for a root header; the first
    /// root header's folder, the include folders and the shelf for `""`; the include folders and
    /// the shelf for `<>`.
    fn lists(&mut self, request: &Request<'_>) -> Result<(), PlaceError> {
        for header in request.headers.iter() {
            self.names.push(Dir { root: Root::Package, path: Some(*header) })?;
        }
        self.headers = request.headers.len();
        for include in self.includes {
            self.names.push(Dir { root: Root::Package, path: Some(RelPath::new(include.name)?) })?;
        }
        self.package.push(Dir { root: Root::Package, path: None })?;
        let first_dir = request.headers.first().and_then(|h| h.parent());
        self.quoted.push(Dir { root: Root::Package, path: first_dir })?;
        for list in [&mut self.quoted, &mut self.angled] {
            for dir in request.include.iter() {
                list.push(Dir { root: Root::Package, path: Some(*dir) })?;
            }
            list.push(Dir { root: Root::Shelf, path: None })?;
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "model-front-end"))]
impl<'a, B: FnOnce(&mut DeclsBuilder<'a>) -> Result<(), ModelError>> CFrontEnd<'a> for ModelFrontEnd<'a, B> {
    fn step(&mut self, _now: u64, input: FrontInput<'_>) -> Result<(), FrontError> {
        match (self.state, input) {
            (FrontState::Begin, FrontInput::Begin { request, target, dialect: _ }) => {
                if let Err(e) = self.lists(request) {
                    let error = self.error(0, FrontFault::Path(e));
                    return self.fail(error);
                }
                self.decls = Some(DeclsBuilder::new(crate::model::DeclsOpts { abi: abi_row(&target.abi), alloc: self.alloc }));
                self.state = FrontState::Reading;
                Ok(())
            }
            (FrontState::Reading, FrontInput::File { .. }) => {
                self.next += 1;
                if self.next == self.total() {
                    let build = self.build.take().expect("built once, on the last answer");
                    let decls = self.decls.as_mut().expect("made at Begin");
                    if let Err(e) = build(decls) {
                        return self.fail(self.error(0, FrontFault::Model(e)));
                    }
                    self.state = FrontState::Done;
                }
                Ok(())
            }
            (FrontState::Reading, FrontInput::Missing) => {
                self.next += 1;
                let error = self.error(0, FrontFault::NotFound);
                self.fail(error)
            }
            (state, _) => {
                let error = self.error(0, FrontFault::OutOfState { state });
                self.fail(error)
            }
        }
    }

    fn next_effect(&self) -> Option<FrontEffect<'_>> {
        if self.state != FrontState::Reading {
            return None;
        }
        let name = self.names.as_search().get(self.next)?.path?;
        let search = match self.next < self.headers {
            true => self.package.as_search(),
            false => match self.includes[self.next - self.headers].angled {
                true => self.angled.as_search(),
                false => self.quoted.as_search(),
            },
        };
        Some(FrontEffect::Read { name, search })
    }

    fn next_deadline(&self) -> Option<u64> {
        None
    }

    fn state(&self) -> FrontState {
        self.state
    }

    fn finish(self) -> Result<DeclsBuilder<'a>, FrontError> {
        match (self.state, self.decls) {
            (FrontState::Done, Some(decls)) => Ok(decls),
            (state, _) => Err(FrontError { file: None, at: 0, fault: FrontFault::OutOfState { state } }),
        }
    }
}
