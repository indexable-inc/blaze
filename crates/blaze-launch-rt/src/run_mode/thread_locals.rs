//! The run mode's thread-locals by index (run.md, "Statics in the run", step 4; the forms are
//! blaze-code's `tls`, THREADS.md D2).
//!
//! **What the parent sends.** Each thread-local's TEMPLATE (its initial value) is a read-only
//! datum under the thread-local's own symbol (`Space::Tls`), sent in `Down::Data` before the
//! record naming it and placed as any static is. The descriptor is the child's own (a pthread
//! key and where a thread's table lives are facts of this process): the parent names its
//! symbol and the ender's once, in `Down::ThreadLocals`, before the first record naming it
//! ([`RunMode::tell_thread_locals`]).
//!
//! **The descriptor** ([`RunMode::make_thread_locals`], [`super::Act::ThreadLocals`]): the
//! ender's slot is numbered and stubbed (its body comes on its first call, a thread's exit),
//! the one pthread key of the run is made with the ender's entry thunk as its destructor
//! ([`super::Memory::thread_key`]), and the descriptor (`key`, `root`, `count` 0, `cap`
//! [`CAP`], `vars`) with room for [`CAP`] rows is published as the static under the
//! descriptor's symbol: a record's load of the descriptor reads its word, as for any static.
//!
//! **An index** ([`Made::index_word`]): a record's load site of thread-local `S` is bound on its
//! first placement: `S` gets the next index, its row (`size` and `align` of the template,
//! `init` its address, `init_len` its size) is written, `count` raised, and an 8-byte word
//! holding the index is the site's word. All of it through the write view before the record's
//! slot word is stored, so no thread reads a row or a word before both hold. The access path
//! (blaze-emit pass/tls_index.rs) takes no lock and asks the parent nothing: the table in line,
//! the getter on a miss.
//!
//! What stays refused by name: a second `ThreadLocals`; a record naming a thread-local before
//! `ThreadLocals` ([`RunModeError::ThreadLocalsUntold`]) or one whose template was not sent
//! (`StaticNotTold`); more than [`CAP`] thread-locals; a thread-local's address in a static's
//! value (`value` of `Space::Tls`: a thread-local has no one address).

use blaze_code::record::Symbol as CodeSymbol;
use blaze_code::tls::{CAP, DESC_BYTES, DESC_COUNT, ROW_BYTES, descriptor, var_row};
use blaze_launch::wire::RecordId;
use blaze_launch::wire::run::Symbol as WireSymbol;
use blaze_own::{AVec, Vec};

use super::{Act, MOST_STATICS, Memory, RunMode, RunModeError, StaticAt};

/// The run's thread-locals: not told yet, told (the two symbols), or made.
pub(super) enum ThreadLocals<'a> {
    Untold,
    Told { descriptor: CodeSymbol, ender: CodeSymbol },
    Made(Made<'a>),
}

/// The descriptor made: its address, and each thread-local's index word.
pub(super) struct Made<'a> {
    at: u64,
    /// Sorted by symbol: each thread-local numbered, with the word holding its index.
    vars: AVec<'a, (CodeSymbol, u64)>,
}

impl<'a> ThreadLocals<'a> {
    pub(super) fn new() -> ThreadLocals<'a> {
        ThreadLocals::Untold
    }
}

impl Made<'_> {
    /// The word holding thread-local `symbol`'s index, numbering it on its first use: its row
    /// written from `template` (the static under the same symbol, placed before the record
    /// naming it, or why there is none), `count` raised, then the word stored.
    pub(super) fn index_word(
        &mut self,
        symbol: CodeSymbol,
        template: Result<StaticAt, RunModeError>,
        memory: &mut impl Memory,
    ) -> Result<u64, RunModeError> {
        let pos = match self.vars.binary_search_by_key(&symbol, |&(it, _)| it) {
            Ok(at) => return Ok(self.vars[at].1),
            Err(pos) => pos,
        };
        let template = template?;
        let index = self.vars.len() as u64;
        if index >= CAP {
            return Err(RunModeError::ThreadLocalsFull);
        }
        let row = var_row(template.len, template.align, template.at, template.len);
        memory.write(self.at + DESC_BYTES as u64 + index * ROW_BYTES as u64, &row)?;
        memory.write(self.at + DESC_COUNT as u64, &(index + 1).to_le_bytes())?;
        let word = memory.reserve(8, 8)?;
        memory.write(word, &index.to_le_bytes())?;
        self.vars.insert_shift(pos, (symbol, word));
        Ok(word)
    }
}

impl RunMode<'_> {
    /// `Down::ThreadLocals`: keeps the two symbols and asks the driver to make the descriptor
    /// before the acts after it. A second message is [`RunModeError::ThreadLocalsTwice`].
    pub(super) fn tell_thread_locals(
        &mut self,
        descriptor: WireSymbol,
        ender: WireSymbol,
        out: &mut AVec<'_, Act>,
    ) -> Result<(), RunModeError> {
        match self.thread_locals {
            ThreadLocals::Untold => {
                self.thread_locals =
                    ThreadLocals::Told { descriptor: CodeSymbol(descriptor.0), ender: CodeSymbol(ender.0) };
                out.push(Act::ThreadLocals);
                Ok(())
            }
            ThreadLocals::Told { .. } | ThreadLocals::Made(_) => Err(RunModeError::ThreadLocalsTwice),
        }
    }

    /// Carries [`Act::ThreadLocals`] (the module doc, "The descriptor").
    pub fn make_thread_locals(&mut self, memory: &mut impl Memory) -> Result<(), RunModeError> {
        let ThreadLocals::Told { descriptor: symbol, ender } = self.thread_locals else {
            return Err(RunModeError::NotBuilt { what: "a thread-local descriptor made twice or before it was told" });
        };
        let (slot, new) = self.slots.number(ender)?;
        if new {
            memory.stub(slot)?;
        }
        let key = memory.thread_key(memory.thunk(slot))?;
        let index = u32::try_from(self.statics.len()).ok().filter(|&n| n < MOST_STATICS).ok_or(RunModeError::StaticsFull)?;
        let len = DESC_BYTES + CAP as usize * ROW_BYTES;
        let at = memory.reserve(len, 16)?;
        let pos = match self.statics.binary_search_by_key(&symbol, |&(it, _)| it) {
            Ok(_) => return Err(RunModeError::StaticTwice { symbol: WireSymbol(symbol.0) }),
            Err(pos) => pos,
        };
        // The header alone: the rows are written as indexes are given (zero until then, as the
        // reservation is).
        let header = descriptor(key.key, key.root, 0, CAP, at + DESC_BYTES as u64);
        // In the code area: the program only reads the descriptor; its rows are written by the
        // control thread through the write view ([`super::Memory::write`]).
        memory.publish_static(index, at, &header, super::Room::Code)?;
        self.statics.insert_shift(pos, (symbol, StaticAt { index, at, len: DESC_BYTES as u64, align: 16 }));
        self.thread_locals = ThreadLocals::Made(Made { at, vars: Vec::new_in(self.alloc) });
        Ok(())
    }
}

/// The run's pthread key and its descriptor's `root` word ([`super::Memory::thread_key`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ThreadKey {
    pub key: u64,
    pub root: u64,
}

/// The run's [`Made`], for `record`'s site naming thread-local `symbol`: refused before the
/// descriptor was made (the parent's `ThreadLocals` comes before any record naming one).
pub(super) fn made<'m, 'a>(state: &'m mut ThreadLocals<'a>, record: RecordId, symbol: CodeSymbol) -> Result<&'m mut Made<'a>, RunModeError> {
    match state {
        ThreadLocals::Made(made) => Ok(made),
        ThreadLocals::Untold | ThreadLocals::Told { .. } => Err(RunModeError::ThreadLocalsUntold { record, symbol }),
    }
}

/// The process's side: the key made with `ender` (the ender's entry thunk) as its destructor,
/// and the descriptor's `root` word.
pub(super) fn native_thread_key(ender: u64) -> Result<ThreadKey, RunModeError> {
    let Ok(ender) = usize::try_from(ender) else { return Err(RunModeError::NotBuilt { what: "an ender past the address space" }) };
    if ender == 0 {
        return Err(RunModeError::NotBuilt { what: "a thread-local key with no ender" });
    }
    // SAFETY: `ender` is the entry thunk of the ender's slot, stubbed before this call: the C
    // function `fn(t: *mut Table)` blaze-build's `tls::index::ender` builds, compiled on its
    // first call (the thunk loads the slot word; the stub asks the parent). One pointer
    // argument, no result: the ABI of pthread's destructor type.
    let destructor = unsafe { core::mem::transmute::<usize, unsafe extern "C" fn(*mut core::ffi::c_void)>(ender) };
    #[cfg(target_os = "macos")]
    let key = blaze_sys::posix::thread::key_create(Some(destructor))
        .map_err(|error| RunModeError::ThreadKey { errno: i32::from(error.raw()) })?;
    #[cfg(not(target_os = "macos"))]
    let key = {
        let mut key: libc::pthread_key_t = 0;
        // SAFETY: `key` is a live local the call fills; the destructor lives as long as the
        // process (a thunk in the run's mapping, never unmapped).
        let errno = unsafe { libc::pthread_key_create(&mut key, Some(destructor)) };
        if errno != 0 {
            return Err(RunModeError::ThreadKey { errno });
        }
        u64::from(key)
    };
    match root_word(key) {
        Ok(root) => Ok(ThreadKey { key, root }),
        Err(why) => {
            #[cfg(target_os = "macos")]
            let _ = blaze_sys::posix::thread::key_delete(key);
            #[cfg(not(target_os = "macos"))]
            // SAFETY: the key was made above and no thread holds a value under it yet.
            unsafe { libc::pthread_key_delete(key as libc::pthread_key_t) };
            Err(why)
        }
    }
}

/// Linux's root: this launcher's own thread-local, at one offset from `tpidr_el0` on every
/// thread (the launcher is the executable, so its thread-locals are in the static TLS block).
/// The getter stores a thread's table pointer here (the descriptor's `root` has `ROOT_OWN`).
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[thread_local]
static ROOT: core::cell::Cell<*mut u8> = core::cell::Cell::new(core::ptr::null_mut());

/// The descriptor's `root` word on macOS: `key * 8`, the key's TSD slot from `tpidrro_el0 & !7`
/// (rule 45: the private layout pthread_getspecific itself reads, pinned by blaze-emit's
/// `the_direct_tsd_read_equals_pthread_getspecific`).
#[cfg(target_os = "macos")]
fn root_word(key: u64) -> Result<u64, RunModeError> {
    match key.checked_mul(8) {
        Some(offset) if offset & blaze_code::tls::ROOT_OWN == 0 => Ok(offset),
        _ => Err(RunModeError::TlsRoot { offset: key }),
    }
}

/// The descriptor's `root` word on Linux: `&ROOT - tpidr_el0`, with `ROOT_OWN` set.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn root_word(_key: u64) -> Result<u64, RunModeError> {
    let tp: u64;
    // SAFETY: reading `tpidr_el0` has no effect; EL0 may read it on Linux, where it is the
    // thread pointer the static TLS block is addressed from.
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nomem, nostack, preserves_flags)) };
    let offset = (ROOT.as_ptr() as u64).wrapping_sub(tp);
    // The access sequence clears bit 63 to read the offset: an offset using it cannot be said.
    if offset & blaze_code::tls::ROOT_OWN != 0 {
        return Err(RunModeError::TlsRoot { offset });
    }
    Ok(offset | blaze_code::tls::ROOT_OWN)
}

/// Any other system: the access sequence is AArch64's on macOS or Linux only.
#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_arch = "aarch64"))))]
fn root_word(_key: u64) -> Result<u64, RunModeError> {
    Err(RunModeError::NotBuilt { what: "thread-locals off AArch64 macOS and Linux" })
}
