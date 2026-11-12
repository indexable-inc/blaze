//! The recording context: a step's only door to the world.

use crate::entry::{name_kind, named};
use crate::nest::{Asks, step_seen};
use crate::{Ask, Fact, KernelError, Read, ReadKind, Seen, StepError, World, WorldError};

/// What a step reads the world through. It records each read: the read's kind, its key's
/// bytes, and the hash the read saw. It never hands out the world itself.
pub struct Reads<'w, W: ?Sized> {
    world: &'w W,
    /// The reads' names, one after another: each its kind, then its key's bytes
    /// ([`crate::entry`]), as its entry's read-list keeps it.
    names: Vec<u8>,
    reads: Vec<Recorded>,
    /// The first read that failed. After it, the run keeps nothing, whatever the step does with
    /// the error.
    failed: Option<Failed>,
    /// Whom it asks for another step's value: the kernel's engine. None for a context made
    /// with no kernel behind it, where [`Reads::step`] is refused.
    asks: Option<&'w mut (dyn Asks + 'w)>,
}

/// Why a run keeps nothing.
#[derive(Clone)]
enum Failed {
    World(WorldError),
    /// A step it asked for was refused; `key` is that read's.
    Asked { key: Vec<u8>, error: Box<KernelError> },
}

impl Failed {
    /// As a read after it is refused.
    fn as_world(&self) -> WorldError {
        match self {
            // clone: an error, once for each read after a failed one
            Failed::World(error) => error.clone(),
            Failed::Asked { key, error } => WorldError {
                kind: ReadKind::STEP,
                // clone: an error, once for each read after a failed one
                key: key.clone(),
                // alloc: an error
                why: format!("a step this run asked for was refused: {error}"),
                not_now: false,
            },
        }
    }

    fn as_step(&self) -> StepError {
        match self {
            // clone: an error, once for each read after a failed one
            Failed::World(error) => StepError::World(error.clone()),
            // clone: an error, once for each read after a failed one
            Failed::Asked { error, .. } => StepError::Asked(error.clone()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Recorded {
    start: usize,
    end: usize,
    seen: Seen,
}

/// Why a run's reads make no trace.
pub(crate) enum Unfinished {
    Failed(WorldError),
    /// A step it asked for was refused.
    Asked(Box<KernelError>),
    /// `name`: the read's name, to be read with [`named`].
    Unsteady { name: Vec<u8>, first: Seen, then: Seen },
}

/// The refusal of a world's read under the kernel's own kind.
fn the_kernels(read: Read<'_>) -> WorldError {
    WorldError::new(
        read,
        "kind 65535 is the kernel's (a step's value): ask for a step with `Reads::step`",
    )
}

/// The read a name written here is of.
fn read_of(name: &[u8]) -> Read<'_> {
    named(name).expect("a name written by `Reads` starts with its kind")
}

impl<'w, W: World + ?Sized> Reads<'w, W> {
    /// A context with no kernel behind it: its step can read the world and ask for no step.
    pub(crate) fn new(world: &'w W) -> Reads<'w, W> {
        // alloc: one step's run, freed once it is kept; empty until it reads
        Reads { world, names: Vec::new(), reads: Vec::new(), failed: None, asks: None }
    }

    /// The context of a run the kernel makes: `asks` answers [`Reads::step`].
    pub(crate) fn asking(world: &'w W, asks: &'w mut (dyn Asks + 'w)) -> Reads<'w, W> {
        // alloc: one step's run, freed once it is kept; empty until it reads
        Reads { world, names: Vec::new(), reads: Vec::new(), failed: None, asks: Some(asks) }
    }

    /// The value of `S` of `input` now, recorded as one read: its kind is
    /// [`ReadKind::STEP`], its key `S`'s number and the input's stable bytes ([`Ask::key`]),
    /// and what it saw is the hash of the value's bytes. The kernel looks `S` up, or runs it
    /// with reads of its own and keeps it; none of those reads becomes this run's. So this
    /// run's entry stays a hit while `S`'s value is the same bytes, whatever `S` read to make
    /// them.
    ///
    /// `S` asking for a value that is being computed (this run's, or one that asked for it) is
    /// refused as a cycle, and this run keeps nothing.
    #[blaze::pure(trusted = "the kernel records the step asked for and the hash of the value it gave")]
    pub fn step<S: Ask<World = W>>(&mut self, input: &S::Input) -> Result<Vec<u8>, StepError> {
        if let Some(failed) = &self.failed {
            return Err(failed.as_step());
        }
        let start = self.names.len();
        name_kind(ReadKind::STEP, &mut self.names);
        let key = self.names.len();
        // alloc: the run's buffer of names
        self.names.extend_from_slice(&S::ID.0.to_be_bytes());
        S::key(input, &mut self.names);
        let asked = match self.asks.as_deref_mut() {
            Some(asks) => asks.asked(&self.names[key..]),
            None => Err(KernelError::Unasked { step: S::ID, name: Some(S::NAME) }),
        };
        let asked = match asked {
            Ok(asked) => asked,
            Err(error) => {
                let error = Box::new(error); // alloc: an error
                // alloc: an error
                let key = self.names[key..].to_vec();
                self.names.truncate(start);
                // clone: an error, kept so that the run keeps nothing
                self.failed = Some(Failed::Asked { key, error: error.clone() });
                return Err(StepError::Asked(error));
            }
        };
        #[cfg(test)]
        {
            if crate::nest::planted::is(crate::nest::planted::Bug::ChildReads) {
                self.names.truncate(start);
                let all = asked.reads.len();
                for (name, seen) in asked.reads.iter().take(all.saturating_sub(1)) {
                    let start = self.names.len();
                    self.names.extend_from_slice(name);
                    self.reads.push(Recorded { start, end: self.names.len(), seen: *seen });
                }
                return Ok(asked.value.expect("a step that asks is given the value's bytes"));
            }
        }
        let seen = step_seen(&self.names[key..], asked.hash);
        self.reads.push(Recorded { start, end: self.names.len(), seen });
        Ok(asked.value.expect("a step that asks is given the value's bytes"))
    }

    /// `F` of `key`, recorded.
    #[blaze::pure(trusted = "the kernel records every read: its kind, its key and the hash it saw")]
    pub fn get<F: Fact<W>>(&mut self, key: &F::Key) -> Result<F::Value<'w>, WorldError> {
        if let Some(failed) = &self.failed {
            return Err(failed.as_world());
        }
        if F::KIND == ReadKind::STEP {
            return self.fail(the_kernels(Read { kind: F::KIND, key: &[] }));
        }
        let start = self.names.len();
        name_kind(F::KIND, &mut self.names);
        F::key(key, &mut self.names);
        match F::get(self.world, key) {
            Ok((value, seen)) => {
                self.reads.push(Recorded { start, end: self.names.len(), seen });
                Ok(value)
            }
            Err(error) => {
                self.names.truncate(start);
                self.fail(error)
            }
        }
    }

    /// The hash of `read`, recorded: a read of what something is, with no value.
    #[blaze::pure(trusted = "the kernel records every read: its kind, its key and the hash it saw")]
    pub fn hash(&mut self, read: Read<'_>) -> Result<Seen, WorldError> {
        if let Some(failed) = &self.failed {
            return Err(failed.as_world());
        }
        if read.kind == ReadKind::STEP {
            return self.fail(the_kernels(read));
        }
        match self.world.hash(read) {
            Ok(seen) => {
                let start = self.names.len();
                name_kind(read.kind, &mut self.names);
                self.names.extend_from_slice(read.key);
                self.reads.push(Recorded { start, end: self.names.len(), seen });
                Ok(seen)
            }
            Err(error) => self.fail(error),
        }
    }

    /// How many reads it recorded, repeats counted.
    pub fn len(&self) -> usize {
        self.reads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }

    fn fail<T>(&mut self, error: WorldError) -> Result<T, WorldError> {
        // clone: an error, kept so that the run keeps nothing
        self.failed = Some(Failed::World(error.clone()));
        Err(error)
    }

    /// Its reads sorted by kind and key, each once. A read that failed, or one that saw two
    /// hashes, makes no trace.
    pub(crate) fn finish(self) -> Result<Trace, Unfinished> {
        let Reads { names, mut reads, failed, .. } = self;
        match failed {
            Some(Failed::World(error)) => return Err(Unfinished::Failed(error)),
            Some(Failed::Asked { error, .. }) => return Err(Unfinished::Asked(error)),
            None => {}
        }
        // A name's bytes order as its kind, then its key: the kind is written big-endian.
        reads.sort_by(|a, b| names[a.start..a.end].cmp(&names[b.start..b.end]));
        let mut kept: usize = 0;
        for at in 0..reads.len() {
            let this = reads[at];
            if let Some(last) = kept.checked_sub(1).map(|it| reads[it])
                && names[last.start..last.end] == names[this.start..this.end]
            {
                if last.seen != this.seen {
                    return Err(Unfinished::Unsteady {
                        // alloc: an error
                        name: names[this.start..this.end].to_vec(),
                        first: last.seen,
                        then: this.seen,
                    });
                }
                continue;
            }
            reads[kept] = this;
            kept += 1;
        }
        reads.truncate(kept);
        Ok(Trace { names, reads })
    }
}

/// A run's reads, sorted by kind and key, each once: what its entry records.
///
/// The kernel makes one of a step it runs ([`Reads`]). A wiring builds one read by read
/// ([`Trace::new`], [`Trace::read`]) for a step that ran where the kernel cannot run it
/// ([`crate::Kernel::keep`]).
#[derive(Debug, Default)]
pub struct Trace {
    names: Vec<u8>,
    reads: Vec<Recorded>,
}

/// Why [`Trace::read`] added nothing: the trace holds the read with another hash. One run saw
/// one read as two things, so the world changed under it and its value is of neither.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TwoSeen {
    pub kind: ReadKind,
    pub key: Vec<u8>,
    pub first: Seen,
    pub then: Seen,
}

impl TwoSeen {
    pub fn read(&self) -> Read<'_> {
        Read { kind: self.kind, key: &self.key }
    }
}

impl Trace {
    /// A trace with no reads.
    pub fn new() -> Trace {
        // alloc: one run's reads, freed once it is kept; empty until a read is added
        Trace { names: Vec::new(), reads: Vec::new() }
    }

    /// Adds a read made elsewhere and what it saw, at its place by kind and key. A read the
    /// trace has with the same hash is kept once; with another hash it is refused.
    pub fn read(&mut self, read: Read<'_>, seen: Seen) -> Result<(), TwoSeen> {
        let kind = read.kind.0.to_be_bytes();
        let names = &self.names;
        let found = self.reads.binary_search_by(|it| {
            names[it.start..it.end].iter().cmp(kind.iter().chain(read.key))
        });
        match found {
            Ok(at) if self.reads[at].seen == seen => Ok(()),
            Ok(at) => Err(TwoSeen {
                kind: read.kind,
                // alloc: an error
                key: read.key.to_vec(),
                first: self.reads[at].seen,
                then: seen,
            }),
            Err(at) => {
                let start = self.names.len();
                name_kind(read.kind, &mut self.names);
                // alloc: the trace's names, one buffer a run
                self.names.extend_from_slice(read.key);
                // alloc: the trace's reads, one buffer a run
                self.reads.insert(at, Recorded { start, end: self.names.len(), seen });
                Ok(())
            }
        }
    }

    pub fn len(&self) -> usize {
        self.reads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (Read<'_>, Seen)> + Clone + '_ {
        self.reads.iter().map(|it| (read_of(&self.names[it.start..it.end]), it.seen))
    }

    /// Each read's name, in the trace's order: what its entry's read-list keeps.
    pub(crate) fn names(&self) -> impl ExactSizeIterator<Item = &[u8]> + Clone + '_ {
        self.reads.iter().map(|it| &self.names[it.start..it.end])
    }

    /// What each read saw, in the trace's order.
    pub(crate) fn seen(&self) -> impl ExactSizeIterator<Item = Seen> + Clone + '_ {
        self.reads.iter().map(|it| it.seen)
    }
}
