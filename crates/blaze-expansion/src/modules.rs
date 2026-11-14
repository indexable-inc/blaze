//! A proc-macro crate's compiled module as a record, and the seam through which a call reaches
//! it without the front end writing anything.
//!
//! **Key** ([`module_key`], kind [`ExpanderModule`]): the toolchain and the macro crate's unit
//! key, made before anything compiles from what its build reads (blaze-macro-eval's `code.rs`:
//! the crate's source closure, its dependencies' keys, the wasm target's settings). A module
//! reads nothing outside its key, so it has no read list, and its record is filed under its key
//! itself.
//!
//! **Value** ([`ModuleValue`]): the module's content digest (what a call's key names, so two
//! unit keys that compile to the same bytes share every call record), the digest and length of
//! its bytes as a blob of the store, and its exports: each macro's name, kind, index and, for a
//! derive, its helper attributes.
//!
//! **The seam** ([`Modules`]): the front end's database holds, for each macro crate, only its
//! unit key, an input set when the crate graph is, before any module exists. A call's expansion
//! asks the seam for the module by that key: a hit is a read of the store; a miss parks the
//! asking task alone on the key's flight while one task compiles it. Nothing is written into
//! the database when the module appears, so no reader is cancelled or waits on a write lock;
//! the only tasks that wait are those whose expansions use that module (shown by the seam's
//! simulation in `tests.rs`). Today's `ProcMacros::give(&mut Db, ...)` (hir-expand's
//! `proc_macro.rs`, called from blaze-cargo's `serve/unit.rs` and `serve/job.rs`'s
//! `Ask::Macros`) is that write, and goes.

use std::{
    alloc::{Allocator, Global},
    fmt,
    hash::Hash,
    marker::PhantomData,
};

use blaze_keyed::{
    kind::{CodeKey, ExpanderModule, Key, Kind, ModuleInputs, Namespace, ProcKind, ToolId},
    proof::{self, Holds, Snapshot},
    record,
    store::{Put as Filing, Store},
};

use crate::{
    Digest, Subject,
    flat::{Corrupt, Reader, Why, Writer},
    machine::{Begin, Flights, Refusal, Ticket, Woken, read_back, record_error, store_error},
};

/// A module's key: the toolchain and its macro crate's code key.
pub fn module_key(tool: ToolId, unit: CodeKey) -> Key<ExpanderModule> {
    ExpanderModule::key(&ModuleInputs { tool, unit, of: PhantomData })
}

/// The digest a module's bytes are kept under, which a call's key names.
pub fn module_digest(bytes: &[u8]) -> Digest {
    Digest::of("blaze-expansion wasm module 1", bytes)
}

/// One macro a module exports.
#[derive(Clone, Copy, Debug)]
pub struct Export<'a> {
    pub name: &'a str,
    pub kind: ProcKind,
    /// Its index in the module's export table, which a call's key names.
    pub index: u32,
    /// A derive's helper attributes.
    pub helpers: &'a [&'a str],
}

/// Writes a module's value.
pub struct ModuleValue<'w> {
    pub module: Digest,
    pub len: u64,
    pub exports: &'w [Export<'w>],
}

fn kind_byte(kind: ProcKind) -> u8 {
    kind as u8
}

fn kind_of(byte: u8) -> Option<ProcKind> {
    Some(match byte {
        0 => ProcKind::Bang,
        1 => ProcKind::Attr,
        2 => ProcKind::Derive,
        _ => return None,
    })
}

impl ModuleValue<'_> {
    pub fn encode(&self) -> Vec<u8, Global> {
        let mut w = Writer::new::<ExpanderModule>();
        w.digest(&self.module).u64(self.len).u32(self.exports.len() as u32);
        for export in self.exports {
            w.str(export.name).u8(kind_byte(export.kind)).u32(export.index);
            w.u32(export.helpers.len() as u32);
            for helper in export.helpers {
                w.str(helper);
            }
        }
        w.finish()
    }
}

/// An export as a module's record holds it.
#[derive(Clone)]
pub struct ExportRef<'a> {
    pub name: &'a str,
    pub kind: ProcKind,
    pub index: u32,
    helper_count: u32,
    helpers: Reader<'a>,
}

impl<'a> ExportRef<'a> {
    pub fn helpers(&self) -> impl Iterator<Item = &'a str> + 'a {
        let mut r = self.helpers.clone();
        (0..self.helper_count).map(move |_| r.str().expect("checked at read"))
    }

    fn read(r: &mut Reader<'a>) -> Result<ExportRef<'a>, Corrupt> {
        let name = r.str()?;
        let byte = r.u8()?;
        let kind = kind_of(byte).ok_or(r.corrupt(Why::Tag(byte)))?;
        let index = r.u32()?;
        let helper_count = r.u32()?;
        let helpers = r.clone();
        for _ in 0..helper_count {
            r.str()?;
        }
        Ok(ExportRef { name, kind, index, helper_count, helpers })
    }
}

impl fmt::Debug for ExportRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportRef")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

/// A module's value, read in place.
#[derive(Clone)]
pub struct ModuleValueRef<'a> {
    pub module: Digest,
    pub len: u64,
    export_count: u32,
    exports: Reader<'a>,
}

impl<'a> ModuleValueRef<'a> {
    pub fn read(bytes: &'a [u8]) -> Result<ModuleValueRef<'a>, Corrupt> {
        let mut r = Reader::open::<ExpanderModule>(bytes)?;
        let module = r.digest()?;
        let len = r.u64()?;
        let export_count = r.u32()?;
        let exports = r.clone();
        for _ in 0..export_count {
            ExportRef::read(&mut r)?;
        }
        r.end()?;
        Ok(ModuleValueRef { module, len, export_count, exports })
    }

    pub fn exports(&self) -> impl Iterator<Item = ExportRef<'a>> + 'a {
        let mut r = self.exports.clone();
        (0..self.export_count).map(move |_| ExportRef::read(&mut r).expect("checked at read"))
    }

    /// The export of this name. A module exports a handful of macros: a scan.
    pub fn export(&self, name: &str) -> Option<ExportRef<'a>> {
        self.exports().find(|it| it.name == name)
    }
}

/// What the seam says to a task that needs a module.
pub enum Resolved<'s, H> {
    /// The module's record, read from the store into the task's frame, with the core's proof
    /// that it is the record of the key asked: [`open_module`] reads its value in place.
    Ready { holds: Holds<'s, ExpanderModule>, record: H },
    /// Compile it (this task is the key's one compiler this request), [`file_module`] its
    /// value, and [`Modules::end`] the ticket.
    Compile(Ticket<ExpanderModule>),
    /// Another task compiles it: park this task alone on the key.
    Wait,
}

impl<H> fmt::Debug for Resolved<'_, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Resolved::Ready { holds, .. } => write!(f, "Ready({holds:?})"),
            Resolved::Compile(ticket) => write!(f, "Compile({:?})", ticket.key),
            Resolved::Wait => f.write_str("Wait"),
        }
    }
}

impl fmt::Debug for ModuleValueRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ModuleValueRef({}, {} exports)", self.module, self.export_count)
    }
}

/// The seam: modules by unit key, read from the store, compiled once per key on a miss. The
/// store is the request's immutable view; the flights are the driver's, behind its one lock,
/// reached only on a miss.
pub struct Modules<W> {
    flights: Flights<ExpanderModule, W>,
}

impl<W: Copy + Eq + Hash + fmt::Debug> Default for Modules<W> {
    fn default() -> Modules<W> {
        Modules { flights: Flights::default() }
    }
}

/// The value of a module's record `record`, by its proof, read in place through the core's one
/// payload reader ([`proof::payload`]): nothing is checked again.
pub fn open_module<'r>(
    subject: &Subject,
    holds: &Holds<'_, ExpanderModule>,
    record: &'r [u8],
) -> Result<ModuleValueRef<'r>, Refusal> {
    let key = holds.key();
    let payload = proof::payload(holds, record).map_err(record_error(subject, key))?;
    ModuleValueRef::read(payload)
        .map_err(|corrupt| Refusal::Corrupt { subject: subject.clone(), key: key.id(), corrupt })
}

/// Files a compiled module's value under its key: what the compiling task does before
/// [`Modules::end`]. Other bytes under the key are [`Refusal::TwoValues`]: the unit key misses
/// an input of the compile. `namespace` is the macro package's ([`blaze_keyed::kind::namespace_of`]).
pub fn file_module<S: Store>(
    store: &S,
    subject: &Subject,
    key: Key<ExpanderModule>,
    namespace: Namespace,
    value: &ModuleValue<'_>,
) -> Result<(), Refusal> {
    let record = record::write_opaque(&key, &value.encode());
    match store.put::<ExpanderModule>(&key, namespace, &record).map_err(store_error(subject, key))? {
        Filing::Written | Filing::Same => Ok(()),
        Filing::Other => Err(Refusal::TwoValues { subject: subject.clone(), key: key.id() }),
    }
}

impl<W: Copy + Eq + Hash + fmt::Debug> Modules<W> {
    /// The record of the module under `key` with the core's proof of it (its one key check),
    /// without the flights: what every task asks first, touching nothing shared and mutable.
    pub fn read<'s, S: Store, F: Allocator>(
        snap: Snapshot<'s>,
        store: &S,
        subject: &Subject,
        key: Key<ExpanderModule>,
        frame: F,
    ) -> Result<Option<(Holds<'s, ExpanderModule>, S::Held<F>)>, Refusal> {
        let Some(read) = store.read::<ExpanderModule, F>(&key, frame).map_err(store_error(subject, key))? else {
            return Ok(None);
        };
        let holds = proof::hit(snap, key, &read.bytes).map_err(record_error(subject, key))?.holds();
        Ok(Some((holds, read.bytes)))
    }

    /// The module under `key` for `task`: [`Modules::read`], then on a miss the key's flight.
    pub fn resolve<'s, S: Store, F: Allocator + Copy>(
        &mut self,
        snap: Snapshot<'s>,
        store: &S,
        subject: &Subject,
        key: Key<ExpanderModule>,
        task: W,
        frame: F,
    ) -> Result<Resolved<'s, S::Held<F>>, Refusal> {
        if let Some((holds, record)) = Self::read(snap, store, subject, key, frame)? {
            return Ok(Resolved::Ready { holds, record });
        }
        match self.flights.begin(subject, key, task) {
            Begin::Run(ticket) => Ok(Resolved::Compile(ticket)),
            Begin::Wait => Ok(Resolved::Wait),
            Begin::Done(address) => {
                let (holds, record) = read_back(snap, store, subject, key, address, frame)?;
                Ok(Resolved::Ready { holds, record })
            }
            Begin::Failed(refusal) => Err(refusal),
        }
    }

    /// Ends a compile: `Ok` once [`file_module`] filed its record under the ticket's key, or
    /// why it failed. Names the tasks parked on the key, and only them.
    pub fn end(
        &mut self,
        ticket: Ticket<ExpanderModule>,
        outcome: Result<(), Refusal>,
    ) -> Woken<ExpanderModule, W> {
        let key = ticket.key;
        self.flights.end(ticket, outcome.map(|()| key))
    }
}
