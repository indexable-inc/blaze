//! The store as the kernel reads and writes it, and as `blaze_record` is handed it
//! ([`Looking`], [`Keeping`]).

use std::cell::{Ref, RefCell};

use blaze_cas::{Guard, Key, Kind, Mismatch, Reader, Store, Writer};
use blaze_ids::ContentId;

/// Reads values, verified.
pub trait Get {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String>;

    /// Whether it has `key`. Lookup probes with this, then reads (verified) only what it takes.
    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(self.get(key)?.is_some())
    }
}

/// Keeps values. A put is readable through its own [`Get`] at once. A key it already has with
/// other bytes is a [`Mismatch`], and it keeps the first.
pub trait Put: Get {
    fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<(), Mismatch>;
}

impl Get for Store {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        // alloc: an error
        Ok(Store::get(self, key).map_err(|it| it.to_string())?.map(|it| it.bytes))
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(Store::has(self, key))
    }
}

impl Put for Store {
    fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        Store::put(self, key, kind, &bytes).map(drop)
    }
}

/// A store read from a task: a [`Reader`] under the task's guard.
pub struct Reading<'g> {
    pub reader: &'g Reader,
    pub guard: &'g Guard,
}

impl Get for Reading<'_> {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        // alloc: an error
        Ok(self.reader.get(self.guard, key).map_err(|it| it.to_string())?.map(|it| it.bytes))
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(self.reader.has(self.guard, key))
    }
}

/// A store read and written from a task: a worker's [`Writer`], whose puts its [`Reader`]
/// reads at once.
pub struct Writing<'g> {
    pub reader: &'g Reader,
    pub writer: &'g mut Writer,
    pub guard: &'g Guard,
}

impl Get for Writing<'_> {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        // alloc: an error
        Ok(self.reader.get(self.guard, key).map_err(|it| it.to_string())?.map(|it| it.bytes))
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(self.reader.has(self.guard, key))
    }
}

impl Put for Writing<'_> {
    fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        self.writer.put(self.guard, key, kind, &bytes).map(drop)
    }
}

/// Why the store stopped a lookup or a keep that went through `blaze_record`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Stopped {
    /// A read failed.
    Read(String),
    /// A put was refused: the key holds other bytes.
    Refused(Mismatch),
    /// A put through a lookup's store, which keeps nothing.
    ReadOnly,
    /// A put while a value read from the store was still held.
    Held,
}

impl Stopped {
    pub(crate) fn why(&self) -> String {
        match self {
            // clone: an error
            Stopped::Read(why) => why.clone(),
            // alloc: an error
            Stopped::Refused(mismatch) => format!(
                "the put was refused as a second value ({:x} is kept, {:x} was given), yet the store shows no value there",
                mismatch.kept, mismatch.given
            ),
            // alloc: an error
            Stopped::ReadOnly => "a put through a lookup, which keeps nothing".to_owned(),
            // alloc: an error
            Stopped::Held => {
                "a put while a value read from the store was still held".to_owned()
            }
        }
    }
}

/// What the kernel's lookups came to, as `blaze_record` counts them ([`Looking`]'s tally):
/// how many chains of read-lists were walked, how many found an entry, the read-lists read and
/// the reads valued. An entry with no reads is found by its key before any chain is walked,
/// and is not among them.
///
/// One for the process, not one a kernel: [`Looking`] and [`Keeping`] are made where a lookup
/// or a keep is, from a store the kernel is handed each call, and neither the store
/// (`blaze_cas`'s) nor the handles outlive the call to own a count. It belongs with whatever
/// comes to own the kernel's store for a build; until then a process with two kernels counts
/// both here.
static LOOKUPS: blaze_record::Tally = blaze_record::Tally::new();

/// What the kernel's lookups of this process came to since the last call, as `blaze_record`
/// counts them (lookups, hits, read-lists read, reads valued), taken: for the driver that
/// reports a build's.
pub fn take_lookups() -> blaze_record::Counts {
    LOOKUPS.take()
}

/// The store a lookup reads an identity's read-lists through.
pub(crate) struct Looking<'s, G: ?Sized>(pub &'s G);

impl<'s, G: Get + ?Sized> blaze_record::Store for Looking<'s, G> {
    type Held = &'s [u8];
    type Error = Stopped;

    fn get(&self, _: blaze_record::Kind, key: &ContentId) -> Result<Option<&'s [u8]>, Stopped> {
        self.0.get(&Key(*key)).map_err(Stopped::Read)
    }

    fn put(&self, _: blaze_record::Kind, _: ContentId, _: &[u8]) -> Result<(), Stopped> {
        Err(Stopped::ReadOnly)
    }

    fn bytes<'h>(held: &'h &'s [u8]) -> &'h [u8] {
        held
    }

    fn tally(&self) -> &blaze_record::Tally {
        &LOOKUPS
    }
}

/// The store a keep files an entry and its read-list through: `blaze_record` reads and puts
/// through one shared handle, and a kernel store puts through `&mut`, so the store is lent
/// out a call at a time. A value read is held only until the next put (`blaze_record` compares
/// it and lets it go); a put that finds one held is [`Stopped::Held`], never a panic.
pub(crate) struct Keeping<'c, 'p, P: ?Sized> {
    pub store: &'c RefCell<&'p mut P>,
    /// The store kind of the step's entries. A read-list is a [`Kind::Query`].
    pub answers: Kind,
}

impl<'c, P: Put + ?Sized> blaze_record::Store for Keeping<'c, '_, P> {
    type Held = Ref<'c, [u8]>;
    type Error = Stopped;

    fn get(
        &self,
        _: blaze_record::Kind,
        key: &ContentId,
    ) -> Result<Option<Ref<'c, [u8]>>, Stopped> {
        let key = Key(*key);
        let store = self.store.try_borrow().map_err(|_| Stopped::Held)?;
        let mut failed = None;
        let held = Ref::filter_map(store, |store| match Get::get(&**store, &key) {
            Ok(found) => found,
            Err(why) => {
                failed = Some(why);
                None
            }
        });
        match (held, failed) {
            (Ok(held), _) => Ok(Some(held)),
            (Err(_), Some(why)) => Err(Stopped::Read(why)),
            (Err(_), None) => Ok(None),
        }
    }

    fn put(&self, kind: blaze_record::Kind, key: ContentId, bytes: &[u8]) -> Result<(), Stopped> {
        let kind = match kind {
            blaze_record::Kind::List => Kind::Query,
            blaze_record::Kind::Answer => self.answers,
        };
        let mut store = self.store.try_borrow_mut().map_err(|_| Stopped::Held)?;
        // alloc: a kernel store takes the bytes it keeps owned
        Put::put(&mut **store, Key(key), kind, bytes.to_vec()).map_err(Stopped::Refused)
    }

    fn bytes<'h>(held: &'h Ref<'c, [u8]>) -> &'h [u8] {
        held
    }

    /// A keep walks no chain (a placing and a keeping count nothing): the kernel's one all
    /// the same, so a lookup that came to go through this handle would be counted.
    fn tally(&self) -> &blaze_record::Tally {
        &LOOKUPS
    }
}
