//! The jump table's execution log in the run process (fastcompile-ident-select 9): which slots
//! a test called, for `blaze_testsel::Record::of_region`.
//!
//! The linker lays the table's header out as [`exec`] says. An image linked with the log
//! starts with its cell on the log table, every word of which holds its slot's log stub, and
//! its map at a map of its own: the first call through each slot marks the slot there, copies
//! the current generation's word into the log table and lands on it, so the prelude (startup,
//! initializers, the harness) is marked as it runs. [`__blaze_exec_arm`], in a forked child
//! before its test, makes the server's region the map, carries the prelude's marks over, and
//! puts every log stub back so the test's own first calls are marked in the region.
//!
//! A slot written while the cell is on the log table goes to the generation's tables, never
//! to the log table ([`publish`]): a log stub not yet called stays in place, so that
//! function's first call is still marked. A log stub that read the generation's word before
//! the write lands on the old target; for a lazy function that is its entry, whose resolver
//! answers at once with the published address. A switch of generations while on the log
//! table changes only the generation's table ([`super::switch`]): slots marked stay marked,
//! and unmarked ones still mark. So a slot first called before a switch keeps the target it
//! copied, for the rest of the process.

#![allow(
    unsafe_code,
    reason = "the execution log's header and region, at the addresses the linker placed them"
)]

use std::{
    ops::Range,
    sync::{
        OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use super::{
    image, os,
    switch::{Layout, POOL},
};
use crate::patch::{DESCRIPTOR, Descriptor, exec};

/// What [`__blaze_exec_arm`] returns: 0 once armed, else why not.
pub const ARMED: i32 = 0;
/// The program has no jump table with an execution log this runtime can find: no live
/// descriptor, or an image linked without `exec_log`.
pub const NO_LOG: i32 = 1;
/// The region is null or shorter than `16 + slots` bytes.
pub const SHORT: i32 = 2;
/// The region's first 8 bytes are not [`exec::MAGIC`].
pub const NOT_A_RECORD: i32 = 3;
/// The region's header names another slot count than the table's.
pub const OTHER_SLOTS: i32 = 4;

/// The tables of the switch's pool ([`POOL`]), by their address in memory (0 for none yet), for
/// [`publish`] to write besides the loader's table: one release store makes each visible.
pub(crate) struct Tables {
    pooled: [AtomicU64; POOL],
}

impl Tables {
    pub(crate) const fn new() -> Tables {
        Tables { pooled: [const { AtomicU64::new(0) }; POOL] }
    }

    /// Makes the pool's table `index`, of `slots` words at `table`, one that [`publish`]
    /// writes. Its words are zero until the switch copies them, which it does by
    /// compare-and-swap from zero, so a word published meanwhile is kept.
    pub(crate) fn register(&self, index: usize, table: u64) {
        self.pooled[index].store(table, Ordering::SeqCst);
    }

    fn each(&self) -> impl Iterator<Item = u64> + '_ {
        self.pooled.iter().map(|it| it.load(Ordering::SeqCst)).filter(|it| *it != 0)
    }
}

/// The process's switch's tables ([`super::Live::start`] makes the one switch there is).
pub(crate) static TABLES: Tables = Tables::new();

/// The jump table's header in memory, read once: where it is, its slot count, where its log
/// stubs are and the loader's table. 40 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Header {
    at: u64,
    slots: u32,
    /// Slot 0's log stub and the bytes from one to the next: both 0 without a log.
    stubs: u64,
    stride: u64,
    loader: u64,
}

const _: () = assert!(size_of::<Header>() == 40);

impl Header {
    /// Reads the fixed words of the header at `at`; the slot count and stride are the low and
    /// high halves of one little-endian word, as on both hosts.
    ///
    /// # Safety
    ///
    /// `at` is 8-aligned and `at..at + exec::HEADER` is mapped readable and writable for the
    /// life of the program, accessed only atomically.
    unsafe fn read(at: u64) -> Header {
        // SAFETY: the caller's.
        let word = |offset: u64| unsafe { image::cell(at + offset) }.load(Ordering::Acquire);
        let fields = word(exec::SLOTS);
        let stubs = word(exec::STUBS);
        Header {
            at,
            slots: fields as u32,
            stubs,
            stride: if stubs == 0 { 0 } else { fields >> 32 },
            loader: word(exec::LOADER),
        }
    }

    /// The header at `at` of a table of `slots` slots, checked: in the image, of `slots`
    /// slots, its loader's table 8-aligned and in the image, and its log (when it has one)
    /// too, each by `in_image`, which names what it refuses.
    ///
    /// # Safety
    ///
    /// `at` is 8-aligned, and `in_image` accepts only ranges of the image's writable data
    /// mapped for the life of the program, accessed only atomically and by the log stubs.
    pub(crate) unsafe fn checked(
        at: u64,
        slots: u64,
        in_image: impl Fn(&Range<u64>) -> Result<(), String>,
    ) -> Result<Header, String> {
        in_image(&(at..at + exec::HEADER))?;
        // SAFETY: the fixed words are in the image (checked above) and 8-aligned, the
        // caller's; the log and the loader's table are checked below, before any use.
        let header = unsafe { Header::read(at) };
        if u64::from(header.slots) != slots {
            // alloc: error path
            return Err(format!(
                "the jump table at {at:#x} has {} slots, where {DESCRIPTOR} says {slots}",
                header.slots
            ));
        }
        let loader = header.loader;
        if loader == 0 || !loader.is_multiple_of(8) {
            // alloc: error path
            return Err(format!(
                "the jump table at {at:#x} holds {loader:#x} for its loader's table"
            ));
        }
        let end = slots.checked_mul(8).and_then(|len| loader.checked_add(len));
        // alloc: error path
        let end =
            end.ok_or_else(|| format!("a jump table at {loader:#x} past the end of memory"))?;
        in_image(&(loader..end))?;
        if header.logged() {
            in_image(&(at..at + exec::len(slots, true)))?;
            if header.stride == 0 {
                // alloc: error path
                return Err(format!("the jump table at {at:#x} has log stubs 0 bytes apart"));
            }
        }
        Ok(header)
    }

    /// The header of the image this code is in, found by its live descriptor and checked
    /// ([`Header::checked`]).
    fn find() -> Result<Header, String> {
        let (near, bytes) = os::descriptor()?;
        let descriptor = Descriptor::decode(bytes)
            // alloc: error path
            .ok_or_else(|| format!("{DESCRIPTOR} at {near:#x} is not a descriptor"))?;
        let layout = Layout::new(&descriptor, near)?;
        // SAFETY: `layout.cell()` is 8-aligned (`Layout::new` checked), and the platform
        // accepts only ranges of the image that has the descriptor; the header and loader's
        // table it names are the linker's `Class::Table`, writable data in every format.
        unsafe { Header::checked(layout.cell(), layout.slots, |it| os::in_image_of(it, near)) }
    }

    /// The header of this process's image, found at the first call and kept.
    pub(crate) fn this() -> Result<Header, String> {
        if let Some(header) = FOUND.get() {
            return Ok(*header);
        }
        let header = Header::find()?;
        Ok(*FOUND.get_or_init(|| header))
    }

    pub(crate) fn slots(&self) -> u32 {
        self.slots
    }

    pub(crate) fn logged(&self) -> bool {
        self.stubs != 0
    }

    /// The loader's table, which holds the generation the image was linked at.
    pub(crate) fn loader(&self) -> u64 {
        self.loader
    }

    fn word(&self, offset: u64) -> &'static AtomicU64 {
        // SAFETY: `offset` is one of the fixed words, or the log table's when `logged`, which
        // `checked` checked are the image's writable data, 8-aligned and accessed only
        // atomically and by the log stubs (whose stores go to the map, not to these words).
        unsafe { image::cell(self.at + offset) }
    }

    /// Makes `region` (`len` bytes) the map and every slot's log stub its log table's word
    /// again, with `region`'s header checked first: [`ARMED`] or why not. The current map's
    /// marks are carried over first, after the map moved, so a call made meanwhile, from this
    /// very code, is marked in one or the other.
    ///
    /// # Safety
    ///
    /// `region..region + len` is mapped readable and writable for as long as the process
    /// runs, and nothing but log stubs and this process's forks writes it meanwhile.
    pub(crate) unsafe fn arm(&self, region: *mut u8, len: usize) -> i32 {
        if !self.logged() {
            return NO_LOG;
        }
        let slots = u64::from(self.slots);
        let need = exec::REGION_HEADER + slots;
        if region.is_null() || (len as u64) < need {
            return SHORT;
        }
        let start = region as u64;
        let byte = |at: u64| {
            // SAFETY: `at` is within the region (each caller's offset is below `need`), the
            // caller's.
            unsafe { image::byte(start + at) }
        };
        if (0..8).any(|at| byte(at).load(Ordering::Relaxed) != exec::MAGIC[at as usize]) {
            return NOT_A_RECORD;
        }
        let named = u32::from_le_bytes([8, 9, 10, 11].map(|at| byte(at).load(Ordering::Relaxed)));
        if named != self.slots {
            return OTHER_SLOTS;
        }
        let before = self.word(exec::MAP).swap(start, Ordering::Relaxed);
        if before != start {
            // SAFETY: `before` is the image's private map or a region armed before, both of
            // `need` bytes mapped for the life of the process.
            unsafe { carry(before, start, exec::REGION_HEADER..need) };
        }
        for slot in 0..slots {
            let stub = self.stubs + slot * self.stride;
            self.word(exec::log_word(slot)).store(stub, Ordering::Relaxed);
        }
        // The cell is on the log table already: the linker starts it there and the switch
        // never moves it in an image with a log.
        let armed = self.slots.to_le_bytes();
        if (start + 12).is_multiple_of(4) {
            // SAFETY: bytes 12..16 of the region, 4-aligned (checked above), the caller's.
            let word = unsafe { AtomicU32::from_ptr((start + 12) as *mut u32) };
            word.store(u32::from_ne_bytes(armed), Ordering::Release);
        } else {
            for (at, value) in (12..15).zip(armed) {
                byte(at).store(value, Ordering::Relaxed);
            }
            byte(15).store(armed[3], Ordering::Release);
        }
        ARMED
    }

    /// Stores `target` in `slot`'s word of the loader's table and of every table of `tables`,
    /// each by one sequentially consistent store (a release store, ordered before the load of
    /// `tables` for [`super::switch::Switch`]'s copy of a new table), and never in the log
    /// table's.
    pub(crate) fn publish(&self, tables: &Tables, slot: u32, target: u64) -> Result<(), String> {
        if slot >= self.slots {
            // alloc: error path
            return Err(format!("slot {slot} published, where the jump table has {}", self.slots));
        }
        let offset = u64::from(slot) * 8;
        // SAFETY: the loader's table is `slots` words of the image's writable data, 8-aligned
        // (`checked` checked), accessed only atomically.
        unsafe { image::cell(self.loader + offset) }.store(target, Ordering::SeqCst);
        for table in tables.each() {
            // SAFETY: a table of the switch's pool, `slots` atomic words never freed
            // (`Switch`'s `Drop` leaks them).
            unsafe { image::cell(table + offset) }.store(target, Ordering::SeqCst);
        }
        Ok(())
    }
}

static FOUND: OnceLock<Header> = OnceLock::new();

/// Marks in the map at `to` each slot marked in the map at `from`, over `bytes` of both: a
/// word at a time where both are 8-aligned there, a byte at a time elsewhere.
///
/// # Safety
///
/// Both ranges are mapped readable and writable, accessed meanwhile only atomically or by the
/// log stubs' byte stores.
unsafe fn carry(from: u64, to: u64, bytes: Range<u64>) {
    let mut at = bytes.start;
    if (from + at).is_multiple_of(8) && (to + at).is_multiple_of(8) {
        while at + 8 <= bytes.end {
            // SAFETY: 8 bytes of each map, 8-aligned (checked above), the caller's.
            let marks = unsafe { image::cell(from + at) }.load(Ordering::Relaxed);
            if marks != 0 {
                // SAFETY: as above.
                unsafe { image::cell(to + at) }.fetch_or(marks, Ordering::Relaxed);
            }
            at += 8;
        }
    }
    while at < bytes.end {
        // SAFETY: a byte of each map, the caller's.
        if unsafe { image::byte(from + at) }.load(Ordering::Relaxed) != 0 {
            // SAFETY: as above.
            unsafe { image::byte(to + at) }.store(1, Ordering::Relaxed);
        }
        at += 1;
    }
}

/// The jump table's slot count, for the server to write in a region's header before it
/// forks; 0 when the program has none this runtime can find. Call it before forking: the
/// first call finds the table, which a forked child of a threaded process mustn't (it
/// allocates), and [`__blaze_exec_arm`] uses what it found.
#[unsafe(no_mangle)]
pub extern "C" fn __blaze_exec_slots() -> u32 {
    Header::this().map_or(0, |it| it.slots())
}

/// Arms the execution log into `region` (`len` bytes), in a forked child with one thread,
/// before its test: the slots already called in this process are marked there, the region
/// becomes the map, every slot is at its log stub again, the cell points at the log table by
/// one release store, and `armed` (bytes 12..16) is the slot count. [`ARMED`] (0), or
/// [`NO_LOG`], [`SHORT`], [`NOT_A_RECORD`] or [`OTHER_SLOTS`], with nothing changed.
///
/// # Safety
///
/// `region..region + len` is mapped readable and writable for the rest of the process's life
/// (and its forks'), and nothing else writes it while this runs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __blaze_exec_arm(region: *mut u8, len: usize) -> i32 {
    let Ok(header) = Header::this() else { return NO_LOG };
    // SAFETY: the caller's.
    unsafe { header.arm(region, len) }
}

/// Publishes `target` as `slot`'s code in every generation's table of this process's jump
/// table, never in the execution log's table: the loader's table (the generation the image
/// was linked at) and every table of the switch's pool, each by one release store, so a
/// later switch keeps the slot published. Lazy compilation's placer publishes through this,
/// after the code is written.
pub fn publish(slot: u32, target: u64) -> Result<(), String> {
    Header::this()?.publish(&TABLES, slot, target)
}
