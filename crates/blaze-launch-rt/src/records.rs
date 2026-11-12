//! [`Loaded`]: the records of a running program, and where what they name lives.
//!
//! The daemon sends each record's bytes ([`Records::record`]) before the message that names
//! it; a record is checked against its id (the BLAKE3 hash of its bytes) when it arrives and
//! kept until the next edit or definition of statics was carried out. Placing a function's
//! record (`blaze_launch::place`) needs an address for every place it names; this is where
//! each comes from, and where it lives for the life of the process:
//!
//! | a record names | it is | where |
//! |---|---|---|
//! | a function | its slot's word (a call) or entry thunk (its address) | the launcher's one mapping, before the code area (`drive`) |
//! | its own data: a string, a constant, a vtable | placed once a process by the datum's id and **never freed**: the program may keep its address past the body that named it | the data mapping ([`sys::Data`]) |
//! | a static | its memory, given when the daemon defines it ([`Records::statics`]), with its initial value; one address for the life of the process | the data mapping |
//! | a thread-local static | a [`tls::Var`]; each thread's copy is made on first touch (`tls`) | the launcher's heap |
//! | an outside symbol | `dlsym` of its name in the process, asked once a name; a symbol that is not there is a refusal naming it (a weak one is 0) | the system's libraries |
//!
//! Nothing here is linked, so nothing is ever moved: an address handed to code is good until
//! the process ends. What is given back is a freed body's unwind entry
//! ([`Records::forget`]); its bytes in the data mapping are not reused (some 100 bytes a
//! body version).
//!
//! Code reaches data only through absolute addresses in its record's pool (tier 0 code
//! takes every data address from its GOT), so the data mapping may lie anywhere.

use std::collections::BTreeMap;
use std::ffi::CString;

use blaze_launch::place::{self, Bound};
use blaze_launch::record::{Place as Named, Record, What};
use blaze_launch::wire::{self, Arch, RecordId};
use blaze_launch::{KEPT, Slot};

use crate::drive::{Place, Records};
use crate::sys::Data;
use crate::{tls, unwind};

/// The address space reserved for a program's data: 4 GiB, committed as it is written.
pub const DATA: usize = 1 << 32;

#[cfg(target_arch = "aarch64")]
const ARCH: Arch = Arch::Aarch64;
#[cfg(target_arch = "x86_64")]
const ARCH: Arch = Arch::X86_64;

/// A static of the program, by its number.
#[derive(Clone, Copy)]
enum Static {
    /// Its address in the data mapping.
    At(u64),
    /// A thread-local: its [`tls::Var`], which lives as long as the process.
    Tls(*const tls::Var),
}

/// What is known of a body a thread can be in: what a backtrace and the unwinder ask.
struct Body {
    len: u64,
    name: Box<str>,
    /// Its unwind entry's address in the data mapping and its FDE's offset in it, once
    /// registered.
    frame: Option<(usize, usize)>,
    registered: bool,
}

/// The records of a running program. See the module's documentation.
pub struct Loaded {
    // A `BTreeMap` by content id, here and below: the launcher takes no hasher crate (it is
    // to be a small binary on bstd), and no order of these maps reaches any output.
    /// The records given and not yet used, by id.
    held: BTreeMap<[u8; 32], Vec<u8>>,
    data: Data,
    /// Where each datum placed so far is, by its id.
    placed: BTreeMap<[u8; 32], u64>,
    /// By number: `None` until defined.
    statics: Vec<Option<Static>>,
    /// The outside symbols asked for so far: the address, or `None` for one that is not in
    /// the process.
    externs: BTreeMap<Box<str>, Option<u64>>,
    /// How many thread-local statics are defined: the next one's place in a thread's table.
    thread_locals: usize,
    /// By the address a body begins at.
    bodies: BTreeMap<u64, Body>,
    /// One datum's or static's bytes on their way into the data mapping.
    scratch: Vec<u8>,
    /// Where the program's backtraces find the names of placed code (`symbols`): made when
    /// the first body is placed, which is when the code mapping's address is first told.
    symbols: Names,
}

#[cfg(target_os = "linux")]
type Names = Option<crate::symbols::Symbols>;
/// Not built on macOS: its standard library asks dyld's image list (`symbols`).
#[cfg(not(target_os = "linux"))]
type Names = ();

// SAFETY: the raw pointers in `statics` are to `tls::Var`s this value leaked: plain data
// that lives as long as the process and is never written again, so they may be read from
// whichever thread owns the `Loaded`.
unsafe impl Send for Loaded {}

/// The hex of an id's first bytes, as a refusal names a record.
fn short(id: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut out = String::new(); // alloc: an error's text
    for byte in &id[..6] {
        // A `String` takes every write.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl Loaded {
    /// Reserves the program's data mapping. No record is held and no static defined.
    pub fn new() -> Result<Loaded, String> {
        let data = Data::map(DATA).map_err(|why| why.to_string())?;
        Ok(Loaded {
            held: BTreeMap::new(),
            data,
            placed: BTreeMap::new(),
            statics: Vec::new(), // alloc: the launcher's tables, for the life of the process
            externs: BTreeMap::new(),
            thread_locals: 0,
            bodies: BTreeMap::new(),
            scratch: Vec::new(), // alloc: as above
            symbols: Names::default(),
        })
    }

    /// The body a program counter or return address `at` is in, when one a thread can be in
    /// holds it: where it begins, and its name.
    pub fn named(&self, at: u64) -> Option<(u64, &str)> {
        let (start, body) = self.bodies.range(..=at).next_back()?;
        (at - start <= body.len).then(|| (*start, &*body.name))
    }

    /// The address of the outside symbol `name`, asked of the process once.
    fn outside(externs: &mut BTreeMap<Box<str>, Option<u64>>, name: &str) -> Option<u64> {
        if let Some(known) = externs.get(name) {
            return *known;
        }
        let found = own(name).or_else(|| {
            let name = CString::new(name).ok()?;
            // SAFETY: a C string that lives across the call; `RTLD_DEFAULT` searches the
            // images of the process in load order, as a linked program's lookup would.
            let at = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            (!at.is_null()).then_some(at as u64)
        });
        externs.insert(name.into(), found);
        found
    }

    /// What each place of `record` is in this process, the numbers of its function and
    /// static places taken from `refs` in order.
    fn bounds(
        record: &Record<'_>,
        refs: wire::List<'_, u32>,
        place: Place,
        statics: &[Option<Static>],
        externs: &mut BTreeMap<Box<str>, Option<u64>>,
    ) -> Result<Vec<Bound>, String> {
        if refs.len() != record.numbered() {
            // alloc: an error
            return Err(format!(
                "`{}` names {} functions and statics and its bind gives {} numbers",
                record.name,
                record.numbered(),
                refs.len()
            ));
        }
        let mut refs = refs.iter();
        // alloc: one function's places, for the length of its placement
        let mut bounds = Vec::with_capacity(record.places.len());
        for named in record.places.iter() {
            // The count was checked above: each numbered place has its number.
            let mut number = || refs.next().unwrap_or(u32::MAX);
            bounds.push(match named {
                Named::Function => {
                    let slot = Slot(number());
                    match (place.word(slot), place.thunk(slot)) {
                        (Some(word), Some(thunk)) => Bound::Function { word, thunk },
                        _ => {
                            // alloc: an error
                            return Err(format!(
                                "`{}` names slot {}, and {} are declared",
                                record.name, slot.0, place.slots
                            ));
                        }
                    }
                }
                Named::Static => {
                    let number = number();
                    match statics.get(number as usize).copied().flatten() {
                        Some(Static::At(address)) => Bound::At(address),
                        #[cfg(target_arch = "aarch64")]
                        Some(Static::Tls(var)) => {
                            Bound::Tls { thunk: tls::thunk as *const () as u64, arg: var as u64 }
                        }
                        #[cfg(not(target_arch = "aarch64"))]
                        Some(Static::Tls(_)) => {
                            // alloc: an error
                            return Err(format!(
                                "`{}` names thread-local static {number}, and the launcher \
                                 has no thread-locals on this instruction set yet",
                                record.name
                            ));
                        }
                        None => {
                            // alloc: an error
                            return Err(format!(
                                "`{}` names static {number}, which the daemon has not defined",
                                record.name
                            ));
                        }
                    }
                }
                Named::Extern(name) => match Loaded::outside(externs, name) {
                    Some(address) => Bound::At(address),
                    None => {
                        // alloc: an error
                        return Err(format!(
                            "`{}` names the outside symbol `{name}`, which no library of \
                             this process defines",
                            record.name
                        ));
                    }
                },
                Named::Weak(name) => Bound::At(Loaded::outside(externs, name).unwrap_or(0)),
            });
        }
        Ok(bounds)
    }

    /// Places the data of `record` that are not placed yet, and gives every datum's address.
    fn data_of(
        record: &Record<'_>,
        bounds: &[Bound],
        data: &mut Data,
        placed: &mut BTreeMap<[u8; 32], u64>,
        scratch: &mut Vec<u8>,
    ) -> Result<Vec<u64>, String> {
        // alloc: one record's data addresses, for the length of its placement
        let mut addresses = Vec::with_capacity(record.data.len());
        for datum in record.data.iter() {
            if let Some(address) = placed.get(&datum.id) {
                addresses.push(*address);
                continue;
            }
            place::place_datum(&datum, bounds, &addresses, scratch)
                .map_err(|why| format!("a datum of `{}`: {why}", record.name))?;
            let address = data
                .reserve(scratch.len(), datum.align as usize)
                .and_then(|address| data.fill(address, scratch).map(|()| address))
                .map_err(|why| format!("a datum of `{}`: {why}", record.name))?;
            placed.insert(datum.id, address as u64);
            addresses.push(address as u64);
        }
        Ok(addresses)
    }

    /// The record `id`, held: decoded, and for this machine.
    fn held<'a>(held: &'a BTreeMap<[u8; 32], Vec<u8>>, id: &RecordId) -> Result<Record<'a>, String> {
        let Some(bytes) = held.get(&id.0) else {
            // alloc: an error
            return Err(format!("record {} was not given before the message that names it", short(&id.0)));
        };
        let record = Record::decode(bytes).map_err(|why| format!("record {}: {why}", short(&id.0)))?;
        if record.arch != ARCH {
            // alloc: an error
            return Err(format!("`{}` is code for {:?}, and this machine is {ARCH:?}", record.name, record.arch));
        }
        Ok(record)
    }
}

/// The symbols a record may name that the launcher itself defines, since no library does.
fn own(name: &str) -> Option<u64> {
    /// What `__cxa_thread_atexit_impl` takes as the object a destructor belongs to: a linked
    /// image's is a symbol its linker makes. The program is one "image" here, never unloaded.
    static DSO_HANDLE: u8 = 0;
    match name {
        "__dso_handle" => Some(&raw const DSO_HANDLE as u64),
        // The program's threads are started through the launcher, which tells the core of
        // each before it runs (`threads`).
        "pthread_create" => Some(crate::threads::create as *const () as u64),
        // The images of the process, with the launcher's code as one more: where the
        // program's backtraces find the names of its own functions (`symbols`).
        #[cfg(target_os = "linux")]
        "dl_iterate_phdr" => Some(crate::symbols::iterate as *const () as u64),
        _ => None,
    }
}

impl Records for Loaded {
    fn record(&mut self, id: RecordId, bytes: &[u8]) -> Result<(), String> {
        let hash = blake3::hash(bytes);
        if *hash.as_bytes() != id.0 {
            // alloc: an error
            return Err(format!(
                "the bytes given as record {} hash to {}",
                short(&id.0),
                short(hash.as_bytes())
            ));
        }
        // alloc: the record's bytes, kept until the message that names it was carried out
        self.held.insert(id.0, bytes.to_vec());
        Ok(())
    }

    fn statics(&mut self, defs: &[wire::StaticDef<'_>], place: Place) -> Result<(), String> {
        let Loaded { held, data, placed, statics, externs, thread_locals, scratch, .. } = self;
        // Every static's memory first, so that their values may name one another.
        for def in defs {
            let record = Loaded::held(held, &def.record)?;
            let What::Static { thread_local, .. } = record.what else {
                // alloc: an error
                return Err(format!("static {} is defined by `{}`, a function's record", def.number, record.name));
            };
            let number = def.number as usize;
            if statics.get(number).is_some_and(Option::is_some) {
                // alloc: an error
                return Err(format!("static {} (`{}`) is defined twice", def.number, record.name));
            }
            if statics.len() <= number {
                statics.resize(number + 1, None);
            }
            let (Ok(size), align) = (usize::try_from(record.size), record.align as usize) else {
                // alloc: an error
                return Err(format!("`{}` is {} bytes, past the address space", record.name, record.size));
            };
            statics[number] = Some(match thread_local {
                false => {
                    let address = data
                        .reserve(size, align)
                        .map_err(|why| format!("the static `{}`: {why}", record.name))?;
                    Static::At(address as u64)
                }
                true => {
                    tls::key().map_err(|errno| {
                        format!("pthread_key_create for the program's thread-locals failed with errno {errno}")
                    })?;
                    // The initial bytes are filled in below, once every static has its
                    // address; the copy they are in lives as long as the process.
                    // alloc: one `Var` a thread-local static, leaked: threads read it for
                    // as long as the process runs.
                    let var = Box::leak(Box::new(tls::Var {
                        index: *thread_locals,
                        size,
                        align,
                        init: core::ptr::NonNull::<u8>::dangling().as_ptr(),
                        init_len: 0,
                    }));
                    *thread_locals += 1;
                    Static::Tls(var)
                }
            });
        }
        for def in defs {
            let record = Loaded::held(held, &def.record)?;
            let bounds = Loaded::bounds(&record, def.refs, place, statics, externs)?;
            let addresses = Loaded::data_of(&record, &bounds, data, placed, scratch)?;
            place::place_static(&record, &bounds, &addresses, scratch)
                .map_err(|why| format!("the static `{}`: {why}", record.name))?;
            match statics[def.number as usize] {
                // Zeroes are what the mapping holds already.
                Some(Static::At(address)) if !record.bytes.is_empty() => data
                    .fill(address as usize, scratch)
                    .map_err(|why| format!("the static `{}`: {why}", record.name))?,
                Some(Static::At(_)) | None => {}
                Some(Static::Tls(var)) if !record.bytes.is_empty() => {
                    // alloc: a thread-local's initial bytes, leaked beside its `Var`
                    let init: &'static [u8] = Box::leak(scratch.clone().into_boxed_slice()); // clone: the bytes kept for the life of the process
                    // SAFETY: `var` is the `Var` leaked above for this static and not yet
                    // handed to any code (its record's binds come after this message), so
                    // this thread alone has it; it is written here once.
                    let var = unsafe { &mut *var.cast_mut() };
                    (var.init, var.init_len) = (init.as_ptr(), init.len());
                }
                Some(Static::Tls(_)) => {}
            }
        }
        Ok(())
    }

    fn code(
        &mut self,
        bind: &wire::Bind<'_>,
        at: Place,
        into: &mut Vec<u8>,
    ) -> Result<(), String> {
        let Loaded { held, data, placed, statics, externs, bodies, scratch, symbols, .. } = self;
        let record = Loaded::held(held, &bind.record)?;
        let pool = place::pool(&record, KEPT).map_err(|why| format!("`{}`: {why}", record.name))?;
        if pool.len != bind.len {
            // alloc: an error
            return Err(format!(
                "`{}` is {} bytes placed, and its bind says {}",
                record.name, pool.len, bind.len
            ));
        }
        let bounds = Loaded::bounds(&record, bind.refs, at, statics, externs)?;
        let addresses = Loaded::data_of(&record, &bounds, data, placed, scratch)?;
        place::place(&record, &pool, at.at, &bounds, &addresses, into)
            .map_err(|why| format!("`{}`: {why}", record.name))?;
        let frame = match &record.unwind {
            None => None,
            Some(entry) => {
                let address = |bound: &Bound| match bound {
                    Bound::Function { thunk, .. } => Some(*thunk),
                    Bound::At(_) | Bound::Tls { .. } => None,
                };
                let personality = entry.personality.and_then(|it| address(&bounds[it as usize]));
                let lsda = entry.lsda.map(|it| addresses[it as usize]);
                let fde = unwind::entry(entry, at.at, record.size, personality, lsda, scratch)
                    .map_err(|why| format!("`{}`: its unwind entry: {why}", record.name))?;
                let address = data
                    .reserve(scratch.len(), 8)
                    .and_then(|address| data.fill(address, scratch).map(|()| address))
                    .map_err(|why| format!("`{}`: its unwind entry: {why}", record.name))?;
                Some((address, fde))
            }
        };
        // Named before it is placed: a backtrace can only ask once a thread is in it.
        #[cfg(target_os = "linux")]
        {
            let symbols = match symbols {
                Some(symbols) => symbols,
                none => none.insert(crate::symbols::Symbols::new(at.words as usize, crate::drive::REACH as usize)?),
            };
            symbols.add(at.at - at.words, bind.len, record.name)?;
        }
        #[cfg(not(target_os = "linux"))]
        let () = *symbols;
        let body = Body { len: bind.len, name: record.name.into(), frame, registered: false };
        bodies.insert(at.at, body);
        Ok(())
    }

    fn placed(&mut self, at: u64, _len: u64) -> Result<(), String> {
        let Some(body) = self.bodies.get_mut(&at) else {
            // alloc: an error
            return Err(format!("the code at {at:#x} was placed and no record was asked for it"));
        };
        if let Some((entry, fde)) = body.frame
            && !body.registered
        {
            // SAFETY: the entry's bytes are in the data mapping, which is never unmapped
            // and whose ranges are written once (`code` wrote these and nothing names them
            // but this); the function it describes was just written at the address it
            // says (`Driver::fill` calls this right after the write).
            unsafe { unwind::register(entry, fde) };
            body.registered = true;
        }
        Ok(())
    }

    fn release(&mut self) {
        self.held.clear();
    }

    fn static_at(&self, number: u32) -> Option<usize> {
        match self.statics.get(number as usize).copied().flatten() {
            Some(Static::At(address)) => Some(address as usize),
            Some(Static::Tls(_)) | None => None,
        }
    }

    fn forget(&mut self, at: u64) {
        if let Some(Body { frame: Some((entry, fde)), registered: true, .. }) = self.bodies.remove(&at) {
            // SAFETY: registered once, in `placed`, and removed from the table here, so
            // deregistered once; the core frees a range only when no thread is or will be
            // in it (`Launch::collect`), or when no word ever named it (an aborted edit).
            unsafe { unwind::deregister(entry, fde) };
        }
    }
}
