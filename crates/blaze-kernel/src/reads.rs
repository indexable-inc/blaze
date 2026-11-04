//! The recording context: a step's only door to the world.

use crate::{Fact, Read, ReadKind, Seen, World, WorldError};

/// What a step reads the world through. It records each read: the read's kind, its key's
/// bytes, and the hash the read saw. It never hands out the world itself.
pub struct Reads<'w, W: ?Sized> {
    world: &'w W,
    /// The keys' bytes, one after another.
    keys: Vec<u8>,
    reads: Vec<Recorded>,
    /// The first read that failed. After it, the run keeps nothing, whatever the step does with
    /// the error.
    failed: Option<WorldError>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Recorded {
    kind: ReadKind,
    start: usize,
    end: usize,
    seen: Seen,
}

/// Why a run's reads make no trace.
pub(crate) enum Unfinished {
    Failed(WorldError),
    Unsteady { kind: ReadKind, key: Vec<u8>, first: Seen, then: Seen },
}

impl<'w, W: World + ?Sized> Reads<'w, W> {
    pub(crate) fn new(world: &'w W) -> Reads<'w, W> {
        // alloc: one step's run, freed once it is kept; empty until it reads
        Reads { world, keys: Vec::new(), reads: Vec::new(), failed: None }
    }

    /// `F` of `key`, recorded.
    #[blaze::pure(trusted = "the kernel records every read: its kind, its key and the hash it saw")]
    pub fn get<F: Fact<W>>(&mut self, key: &F::Key) -> Result<F::Value<'w>, WorldError> {
        if let Some(failed) = &self.failed {
            // clone: an error, once for each read after a failed one
            return Err(failed.clone());
        }
        let start = self.keys.len();
        F::key(key, &mut self.keys);
        match F::get(self.world, key) {
            Ok((value, seen)) => {
                self.reads.push(Recorded { kind: F::KIND, start, end: self.keys.len(), seen });
                Ok(value)
            }
            Err(error) => {
                self.keys.truncate(start);
                self.fail(error)
            }
        }
    }

    /// The hash of `read`, recorded: a read of what something is, with no value.
    #[blaze::pure(trusted = "the kernel records every read: its kind, its key and the hash it saw")]
    pub fn hash(&mut self, read: Read<'_>) -> Result<Seen, WorldError> {
        if let Some(failed) = &self.failed {
            // clone: an error, once for each read after a failed one
            return Err(failed.clone());
        }
        match self.world.hash(read) {
            Ok(seen) => {
                let start = self.keys.len();
                self.keys.extend_from_slice(read.key);
                self.reads.push(Recorded { kind: read.kind, start, end: self.keys.len(), seen });
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
        self.failed = Some(error.clone());
        Err(error)
    }

    /// Its reads sorted by kind and key, each once. A read that failed, or one that saw two
    /// hashes, makes no trace.
    pub(crate) fn finish(self) -> Result<Trace, Unfinished> {
        let Reads { keys, mut reads, failed, .. } = self;
        if let Some(error) = failed {
            return Err(Unfinished::Failed(error));
        }
        reads.sort_by(|a, b| (a.kind, &keys[a.start..a.end]).cmp(&(b.kind, &keys[b.start..b.end])));
        let mut kept: usize = 0;
        for at in 0..reads.len() {
            let this = reads[at];
            if let Some(last) = kept.checked_sub(1).map(|it| reads[it])
                && last.kind == this.kind
                && keys[last.start..last.end] == keys[this.start..this.end]
            {
                if last.seen != this.seen {
                    return Err(Unfinished::Unsteady {
                        kind: this.kind,
                        // alloc: an error
                        key: keys[this.start..this.end].to_vec(),
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
        Ok(Trace { keys, reads })
    }
}

/// A run's reads, sorted by kind and key, each once: what its entry records.
#[derive(Debug)]
pub struct Trace {
    keys: Vec<u8>,
    reads: Vec<Recorded>,
}

impl Trace {
    pub fn len(&self) -> usize {
        self.reads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (Read<'_>, Seen)> + Clone + '_ {
        self.reads
            .iter()
            .map(|it| (Read { kind: it.kind, key: &self.keys[it.start..it.end] }, it.seen))
    }
}
