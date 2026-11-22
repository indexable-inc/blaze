//! Thread-locals by index on the run path (docs: /Volumes/Projects/blaze-int-1112-notes/port27/
//! tempo-run/THREADS.md, design D2; the getter, the ender and the access sequence are
//! blaze-build's `tls::index` and blaze-emit's `pass/tls_index.rs`).
//!
//! Why not one block per thread: under bottom.md 11 a function's code is placed on its first
//! call, so a thread-local first named by a record placed later would have no offset in a block
//! every running thread already allocated, and a block cannot move (`LocalKey::with` hands out
//! `&T` into it). So each thread-local gets an INDEX, numbered by the run ([`TlsVars::index_of`]):
//! a code word naming a thread-local holds its index, and a thread's table holds one pointer per
//! index to that thread's copy, made from the thread-local's row on first touch.
//!
//! What this file makes for a run:
//! - [`TlsVars`]: the numbering. The ladder's whole-program run numbers every thread-local in
//!   item-key order before any body is emitted (run.rs `static_image`), so the descriptor is
//!   written whole at load and the bodies only look indices up. The index is in no key and no
//!   record: a record names a thread-local by symbol (`Space::Tls`).
//! - [`TlsVars::image_parts`]: the descriptor static (D2's 40 bytes, then its rows) and each
//!   thread-local's template, an immutable static under a key derived from the thread-local's
//!   own ([`template_key`]); a row's `init` word is relocated to its template.
//! - [`create_key`]: the run's pthread key, with the ender (the LIR ender's slot thunk) as the
//!   key's destructor, and the descriptor's `root`.
//! - [`RecordRun::ender`]: the ender's code record for the record walk, beside the getter's.
//!
//! The rows are reserved as `cap = count` here, the form D2 gives `export`: a whole-program run
//! places every body before the entry runs, so no thread-local is numbered after the image is
//! written. The lazily placing run (D2's control thread) reserves `tls::index::CAP` rows instead.

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
use core::cell::Cell;
use std::alloc::Global;

use blaze_build::data::{DataAlloc, DataError, DataRecord, DataReloc, DataTarget};
use blaze_build::tls::index;
use blaze_build::tls::{SelfPointer, TlsError};
use blaze_code::record::Symbol;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::ItemKey;

use crate::code_records::{Callee, CodeRecordError, Declared, InstanceNames, KeyParts, RecordRun, make_entry};
use crate::place::PlaceError;
use crate::run::RunError;
use crate::seam::FrontEnd;

/// The domain of a thread-local's template key: stepped when the template's form changes.
const TEMPLATE_DOMAIN: &str = "blaze-ladder thread-local template static 1";

/// The domain of the ender's symbol: one ender a program.
const ENDER_SYMBOL_DOMAIN: &str = "blaze-ladder thread-local ender symbol 1";

/// The domain of the ender's body digest: stepped when blaze-build's `tls::index::ender`
/// changes, as the getter's `GETTER_DOMAIN` is for `tls::index::getter`.
const ENDER_DOMAIN: &str = "blaze-ladder thread-local ender body 1";

/// How a walk's [`crate::code_records::Met`] names the thread-local ender.
pub const ENDER_NAME: &str = "the thread-local ender";

/// The run's numbering of its thread-locals: `order[i]` is the thread-local of index `i`.
pub(crate) struct TlsVars {
    order: Vec<ItemKey, Global>,
}

impl TlsVars {
    pub(crate) fn new() -> TlsVars {
        TlsVars { order: Vec::new_in(Global) }
    }

    /// How many thread-locals are numbered: the descriptor's `count` once
    /// [`image_parts`](TlsVars::image_parts) wrote it.
    pub(crate) fn len(&self) -> u32 {
        // At most `tls::index::CAP` (65,536) keys are ever pushed (`index_of`), so it fits.
        u32::try_from(self.order.len()).unwrap_or(u32::MAX)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The index of thread-local `key`: the one it was given, else the next, numbered on this
    /// first ask. Past `tls::index::CAP` thread-locals a typed refusal naming `key`.
    pub(crate) fn index_of(&mut self, key: ItemKey) -> Result<u32, RunError> {
        let cap = RunError::TlsCap { thread_local: key, cap: index::CAP };
        if let Some(at) = self.order.iter().position(|&k| k == key) {
            return u32::try_from(at).map_err(|_| cap);
        }
        let next = self.order.len() as u64;
        if next >= index::CAP {
            return Err(cap);
        }
        self.order.push(key);
        u32::try_from(next).map_err(|_| RunError::TlsCap { thread_local: key, cap: index::CAP })
    }

    /// The descriptor static of the numbered thread-locals and their templates: each thread-local's
    /// evaluated record (from `values` or the front end's `static_records`) becomes a template
    /// static under [`template_key`], and the descriptor's row of index `i` points at the
    /// template of `order[i]`. Refused by name: a numbered thread-local with no record, one with
    /// two, a template that points into itself.
    pub(crate) fn image_parts<P: FrontEnd>(
        &self,
        program: &P,
        values: &[(ItemKey, DataRecord<Global>)],
    ) -> Result<(DataRecord<Global>, Vec<(ItemKey, DataRecord<Global>), Global>), RunError> {
        let mut found: Vec<(ItemKey, &DataRecord<Global>), Global> = Vec::new_in(Global);
        for (key, record) in values.iter().chain(program.static_records()) {
            if program.static_item(*key).is_some_and(|s| s.thread_local) {
                found.push((*key, record));
            }
        }
        found.sort_unstable_by_key(|f| f.0);
        for pair in found.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(data_error(DataError::Twice { static_: pair[0].0 }));
            }
        }
        self.parts_of(&found)
    }

    /// [`image_parts`](TlsVars::image_parts) over the thread-locals' records, sorted by key and
    /// each key once.
    fn parts_of(
        &self,
        found: &[(ItemKey, &DataRecord<Global>)],
    ) -> Result<(DataRecord<Global>, Vec<(ItemKey, DataRecord<Global>), Global>), RunError> {
        let count = self.order.len() as u64;
        let row_bytes = index::ROW_BYTES as usize;
        let mut head = DataAlloc { bytes: Default::default(), align: 3, mutable: true, relocs: Default::default() };
        head.bytes.resize(index::DESC_BYTES as usize, 0);
        put_word(&mut head.bytes, index::DESC_COUNT as usize, count);
        put_word(&mut head.bytes, index::DESC_CAP as usize, count);
        head.relocs.push(DataReloc { offset: index::DESC_VARS as u32, target: DataTarget::Local(1) });
        // Written once here, before any body that names an index runs: the rows are immutable.
        let mut rows = DataAlloc { bytes: Default::default(), align: 3, mutable: false, relocs: Default::default() };
        rows.bytes.reserve(self.order.len() * row_bytes);
        let mut templates = Vec::with_capacity_in(self.order.len(), Global);
        for (n, &key) in self.order.iter().enumerate() {
            let Ok(at) = found.binary_search_by_key(&key, |f| f.0) else {
                return Err(data_error(DataError::NoStatic { static_: key }));
            };
            let template = template_of(key, found[at].1)?;
            let root = &template.allocs[0];
            let size = root.bytes.len() as u64;
            // At least a pointer's alignment: macOS's `aligned_alloc` refuses less (EINVAL).
            let align = 1u64 << root.align.max(3);
            rows.bytes.extend_from_slice(&index::var_row(size, align, 0, size));
            let offset = n * row_bytes + index::ROW_INIT as usize;
            let offset = u32::try_from(offset).map_err(|_| data_error(DataError::TooLarge))?;
            rows.relocs.push(DataReloc { offset, target: DataTarget::Static(template_key(key)) });
            templates.push((template_key(key), template));
        }
        let mut desc = DataRecord { allocs: Default::default() };
        desc.allocs.push(head);
        desc.allocs.push(rows);
        desc.check().map_err(data_error)?;
        Ok((desc, templates))
    }
}

/// `value` as the little-endian word at byte `at` of `bytes`.
fn put_word(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn data_error(error: DataError) -> RunError {
    RunError::Tls { error: TlsError::Data(error) }
}

/// The key of thread-local `key`'s template static: derived from its own, so no item of the
/// program has it (the image refuses two records under one key, `DataError::Twice`).
fn template_key(key: ItemKey) -> ItemKey {
    let id = ContentId::of(TEMPLATE_DOMAIN, &key.0.to_le_bytes()).0;
    let mut low = [0u8; 16];
    low.copy_from_slice(&id[..16]);
    ItemKey(u128::from_le_bytes(low))
}

/// Thread-local `key`'s evaluated record as its template: the same allocations, the value's
/// immutable (only `memcpy`'d from). A relocation to the value itself (`Local(0)`) would point
/// every thread's copy at the template: refused (`TlsError::SelfPointer`), as E1 refused it.
fn template_of(key: ItemKey, record: &DataRecord<Global>) -> Result<DataRecord<Global>, RunError> {
    record.check().map_err(data_error)?;
    let mut out = DataRecord { allocs: Default::default() };
    for (n, alloc) in record.allocs.iter().enumerate() {
        if alloc.relocs.iter().any(|r| r.target == DataTarget::Local(0)) {
            return Err(RunError::Tls { error: TlsError::SelfPointer(SelfPointer { static_: key }) });
        }
        out.allocs.push(DataAlloc {
            bytes: alloc.bytes.to_vec_in(Global),
            align: alloc.align,
            mutable: alloc.mutable && n != 0,
            relocs: alloc.relocs.to_vec_in(Global),
        });
    }
    Ok(out)
}

/// Linux's root: this launcher's own thread-local, at the same offset from `tpidr_el0` on every
/// thread (the launcher is the executable, so its thread-locals are in the static TLS block).
/// The getter stores a thread's table pointer here (the descriptor's `root` has `ROOT_OWN`).
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[thread_local]
static ROOT: Cell<*mut u8> = Cell::new(core::ptr::null_mut());

/// The descriptor's `root` word: macOS `key * 8` (the key's TSD slot from `tpidrro_el0 & !7`,
/// rule 45, pinned by blaze-emit's `the_direct_tsd_read_equals_pthread_getspecific`).
#[cfg(target_os = "macos")]
fn root_word(key: u64) -> Result<u64, PlaceError> {
    match key.checked_mul(8) {
        Some(offset) if offset & index::ROOT_OWN == 0 => Ok(offset),
        _ => Err(PlaceError::TlsRoot { offset: key }),
    }
}

/// The descriptor's `root` word: Linux `&ROOT - tpidr_el0`, with `ROOT_OWN` set (the getter
/// stores the root itself).
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn root_word(_key: u64) -> Result<u64, PlaceError> {
    let tp: u64;
    // SAFETY: reading `tpidr_el0` has no effect; EL0 may read it on Linux, where it is the
    // thread pointer the static TLS block is addressed from.
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nomem, nostack, preserves_flags)) };
    let offset = (ROOT.as_ptr() as u64).wrapping_sub(tp);
    // The access sequence clears bit 63 to read the offset: an offset using it cannot be said.
    if offset & index::ROOT_OWN != 0 {
        return Err(PlaceError::TlsRoot { offset });
    }
    Ok(offset | index::ROOT_OWN)
}

/// Any other system: the access sequence is AArch64's on macOS or Linux only.
#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_arch = "aarch64"))))]
fn root_word(_key: u64) -> Result<u64, PlaceError> {
    Err(PlaceError::NotThisMachine)
}

/// Creates the run's thread-local key in the descriptor static at `desc` (its `key` word), with
/// the function at `ender` (the ender's slot thunk) as the key's destructor, then writes the
/// descriptor's `root`. Called once on the control thread after placement and before the
/// entry runs, so no thread of the program reads the descriptor yet.
pub fn create_key(desc: u64, ender: u64) -> Result<(), PlaceError> {
    let Ok(ender) = usize::try_from(ender) else { return Err(PlaceError::NoEnder) };
    if ender == 0 || desc == 0 || desc % 8 != 0 {
        return Err(PlaceError::NoEnder);
    }
    type PthreadDtor = unsafe extern "C" fn(*mut core::ffi::c_void);
    const {
        assert!(size_of::<PthreadDtor>() == size_of::<usize>(), "pthread destructor is one address");
        assert!(align_of::<PthreadDtor>() == align_of::<usize>(), "pthread destructor has pointer alignment");
    }
    // SAFETY: `ender` is the entry thunk of the ender's slot, declared by `place` before this
    // call: the C function `fn(t: *mut Table)` blaze-build's `tls::index::ender` built. One
    // pointer argument, no result: the ABI of pthread's destructor type.
    let destructor: PthreadDtor = unsafe { core::ptr::read((&raw const ender).cast::<PthreadDtor>()) };
    #[cfg(target_os = "macos")]
    let key_at = (desc + index::DESC_KEY as u64) as *mut blaze_sys::posix::thread::pthread_key_t;
    #[cfg(not(target_os = "macos"))]
    let key_at = (desc + index::DESC_KEY as u64) as *mut libc::pthread_key_t;
    // SAFETY: `desc` is the descriptor static in the image `load_statics` wrote, 8-aligned
    // (checked above), its `key` word 8 bytes; `pthread_key_t` (4 bytes on Linux, 8 on macOS)
    // fits in it, the rest of the word left zero as the image wrote it.
    #[cfg(target_os = "macos")]
    let key = match blaze_sys::posix::thread::key_create(Some(destructor)) {
        Ok(key) => { unsafe { key_at.write(key) }; key }
        Err(error) => return Err(PlaceError::TlsKey { errno: i32::from(error.raw()) }),
    };
    #[cfg(not(target_os = "macos"))]
    let key = {
        let errno = unsafe { libc::pthread_key_create(key_at, Some(destructor)) };
        if errno != 0 { return Err(PlaceError::TlsKey { errno }); }
        unsafe { key_at.read() }
    };
    // SAFETY: written just above by `pthread_key_create`.
    let root = match root_word(u64::from(key)) {
        Ok(root) => root,
        Err(error) => {
            // SAFETY: the key was created above and no thread has a value under it yet.
            #[cfg(target_os = "macos")]
            { let _ = blaze_sys::posix::thread::key_delete(key); }
            #[cfg(not(target_os = "macos"))]
            { unsafe { libc::pthread_key_delete(key) }; }
            return Err(error);
        }
    };
    // SAFETY: the descriptor's `root` word, 8-aligned inside the 40-byte descriptor; no other
    // thread reads it before the entry runs (threads the program starts later are ordered after
    // this write by `pthread_create`).
    unsafe { ((desc + index::DESC_ROOT as u64) as *mut u64).write(root) };
    Ok(())
}

/// The program's thread-local ender function: one a program.
pub fn tls_ender_symbol() -> Symbol {
    Symbol(ContentId::of(ENDER_SYMBOL_DOMAIN, &[]).0)
}

/// The ender's own key parts: as the getter's (code_records.rs `getter_parts`), made by
/// blaze-build's `tls::index::ender` from no text: its body digest is that generator's form
/// ([`ENDER_DOMAIN`]) and the C functions it calls (`ENDER_IMPORTS`); no ground facts.
pub fn ender_parts() -> KeyParts {
    let mut body = Hasher::new(ENDER_DOMAIN);
    for name in index::ENDER_IMPORTS {
        body.part(name.as_bytes());
    }
    let mut ground = Hasher::new(ENDER_DOMAIN);
    ground.part(b"no ground facts");
    KeyParts { symbol: tls_ender_symbol(), body: body.finish(), ground: ground.finish() }
}

/// The ender's entry: its LIR emitted with its calls named as the C functions they are,
/// declaring no instance.
pub fn make_ender_entry<'a>(isa: blaze_emit::Isa, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, CodeRecordError> {
    let func = index::ender(Global).map_err(CodeRecordError::Ender)?;
    let funcs = index::ENDER_IMPORTS.map(Callee::C);
    let names = InstanceNames { funcs: &funcs, statics: &[], c_statics: &[], vtables: &[], tls: &[] };
    let mut declared: AVec<'_, Declared<'_>> = Vec::new_in(alloc);
    make_entry(func.verified(), &names, crate::front2::builtins_shim::Routines::NONE, &mut declared, isa, alloc)
}

impl RecordRun<'_> {
    /// The thread-local ender's entry ([`ender_parts`]): found in the store, or made and put;
    /// met as [`ENDER_NAME`]. Nothing calls it by a site: the run's key names it as its
    /// destructor ([`create_key`]). `alloc` is the caller's scratch for the entry until it is
    /// put.
    pub fn ender(&mut self, alloc: Alloc<'_>) -> Result<(), CodeRecordError> {
        // The one find-or-make of the run's special records (run/special.rs).
        self.special(crate::run::special::Special::Ender, alloc)
    }
}

#[cfg(test)]
mod tests;
