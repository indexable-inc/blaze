//! The one buffer a function is written into: its bytes, labels for places not written yet
//! (blocks, edges, trap stubs, constants), fixups resolved once every label is bound, and the
//! relocations the linker resolves. Reserved once per function from the sizes of the
//! stencils and the glue's bounds, on the compile's frame: writing never allocates.
//!
//! # Islands (branch relaxation)
//!
//! An AArch64 field reaches only so far: `tbz` ±32 KiB, `b.cond`, `cbz`, `adr` and `ldr`
//! (literal) ±1 MiB, `b` ±128 MiB ([`enc::Reach`]). Stencil bytes are copied whole, so a
//! branch in one can't be rewritten into a longer sequence the way LLVM's branch relaxation
//! moves blocks; instead the buffer keeps every field that may end up past its reach (a
//! *near* fixup: its label unbound, or bound behind it out of reach) with its *deadline*, the
//! last offset its target may be at, and before each piece of code the patcher asks for
//! [`Buf::room`]. When writing the piece could take a near fixup past its deadline before an
//! island serves it, an island is written first, between two pieces:
//!
//! ```text
//! b over            ; the code before falls through it
//! veneer_1: b L1    ; per branch label: the near branches to L1 go here
//! ...
//! .align 16
//! datum_1           ; per data label: the constant or rodata entry, bound here
//! ...
//! over:
//! ```
//!
//! A veneer is the library's `jump` stencil, so it reaches as far as `b` does (and is itself a
//! near fixup in a function long enough for that to matter: veneers chain). A datum's label
//! is bound in the island; a later use out of its reach takes a new label, so the datum is
//! placed again (`patch::Side::konst`). An island serves every near fixup whose deadline it
//! must (before the piece) and every one past half its reach, so islands are rare: one per
//! half reach of the code at most. Which forms are near at all follows from the function's
//! bound ([`Buf::reserve`]): a form reaching past what the code could grow to, islands
//! included, is never near, so a function under 64 KiB tracks nothing and pays one compare
//! per piece. Every field is still checked when it is written ([`Buf::resolve`]): whatever
//! no island reached is an error naming the field, never other code.

use blaze_mem::frame::Frame;
use cranelift_codegen::binemit;

#[cfg(test)]
mod tests;

use crate::{
    Error,
    code::{Name, Reloc},
    enc::{self, Far, Form, Reach},
};

/// A place in the function, bound once.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Label(pub u32);

/// A field to be written with a label's place once it is bound: `S` is the label's offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixup {
    pub at: u32,
    pub form: Form,
    pub label: Label,
    pub addend: i64,
}

/// What an island is written with ([`Buf::room`]): the architecture's unconditional jump (the
/// library's `jump` stencil: its code, and its one field to the target), the no-op padding is
/// made of, and the alignment of the data it places.
#[derive(Clone, Copy, Debug)]
pub struct Island<'a> {
    pub jump: &'a [u8],
    /// Where the jump's field is in it, its form and its addend.
    pub at: u32,
    pub form: Form,
    pub addend: i64,
    pub nop: &'a [u8],
    pub align: u32,
}

/// The most bytes a datum an island places has: a 16-byte constant or rodata entry.
pub const DATUM: u32 = 16;

/// A fixup whose field may not reach its label ([`Buf::room`]): its index in the fixups, and
/// the last offset its label (or a stand-in for it) may be at.
#[derive(Clone, Copy, Debug)]
struct Near {
    fixup: u32,
    deadline: u32,
}

const UNBOUND: u32 = u32::MAX;
/// No deadline: nothing is near.
const NEVER: u32 = u32::MAX;
/// How many times its bound a function's code may grow to with its islands: each serves a
/// field with at most a 16-byte datum or a veneer, plus a jump and its padding, and holds at
/// least one. A form reaching past the bound times this is never near.
const GROWTH: u64 = 16;

pub struct Buf<'f> {
    bytes: Vec<u8, &'f Frame>,
    /// By label: its offset, or `u32::MAX` while unbound.
    labels: Vec<u32, &'f Frame>,
    fixups: Vec<Fixup, &'f Frame>,
    relocs: Vec<Reloc, &'f Frame>,
    /// The fixups that may not reach their labels, in the order made.
    near: Vec<Near, &'f Frame>,
    /// The least deadline of `near`, [`NEVER`] for none.
    due: u32,
    /// The most bytes the code has without islands, from [`Buf::reserve`].
    bound: u32,
    /// The fixups the island being written serves.
    served: Vec<u32, &'f Frame>,
}

impl<'f> Buf<'f> {
    /// An empty buffer on `frame`, the compile's.
    pub fn new_in(frame: &'f Frame) -> Buf<'f> {
        Buf {
            bytes: Vec::new_in(frame),
            labels: Vec::new_in(frame),
            fixups: Vec::new_in(frame),
            relocs: Vec::new_in(frame),
            near: Vec::new_in(frame),
            due: NEVER,
            bound: 0,
            served: Vec::new_in(frame),
        }
    }

    /// The frame the buffer is on, for the compile's other temporaries.
    pub fn temp(&self) -> &'f Frame {
        *self.bytes.allocator()
    }

    /// Room for `bytes` more bytes, `labels` labels and `fixups` fixups: one reservation per
    /// function, from the sizes the patcher adds up. `bytes` is also the bound of the code
    /// that follows, without islands, which decides the forms that may be out of reach.
    pub fn reserve(&mut self, bytes: usize, labels: usize, fixups: usize) {
        self.bytes.reserve(bytes);
        self.labels.reserve(labels);
        self.fixups.reserve(fixups);
        self.bound = u32::try_from(self.bytes.len() + bytes).unwrap_or(u32::MAX);
    }

    /// The offset the next byte is written at.
    #[inline]
    pub fn len(&self) -> u32 {
        self.bytes.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes from `at`, for patching what was just copied.
    #[inline]
    pub fn bytes_mut(&mut self, at: u32) -> &mut [u8] {
        &mut self.bytes[at as usize..]
    }

    #[inline]
    pub fn put(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub fn put1(&mut self, it: u8) {
        self.put(&[it]);
    }

    pub fn put2(&mut self, it: u16) {
        self.put(&it.to_le_bytes());
    }

    pub fn put4(&mut self, it: u32) {
        self.put(&it.to_le_bytes());
    }

    pub fn put8(&mut self, it: u64) {
        self.put(&it.to_le_bytes());
    }

    /// Pads to a multiple of `align` (a power of two) with `fill` repeated: `nop`s (x86-64
    /// `0x90`; AArch64 `d503201f`) in code, so a padded gap is never executed as anything else.
    pub fn align(&mut self, align: u32, fill: &[u8]) {
        debug_assert!(align.is_power_of_two() && !fill.is_empty());
        let pad = blaze_mem::align::padding_for_u32(self.len(), align);
        self.pad(pad, fill);
    }

    /// Writes `count` bytes of `fill` repeated (the last copy cut short).
    pub fn pad(&mut self, count: u32, fill: &[u8]) {
        let mut left = count as usize;
        while left > 0 {
            let n = left.min(fill.len());
            self.bytes.extend_from_slice(&fill[..n]);
            left -= n;
        }
    }

    /// A new unbound label.
    #[inline]
    pub fn label(&mut self) -> Label {
        let label = Label(self.labels.len() as u32);
        self.labels.push(UNBOUND);
        label
    }

    /// Binds `label` to the current offset. A label is bound once.
    #[inline]
    pub fn bind(&mut self, label: Label) {
        debug_assert_eq!(self.labels[label.0 as usize], UNBOUND, "{label:?} bound twice");
        self.labels[label.0 as usize] = self.len();
    }

    #[inline]
    pub fn offset(&self, label: Label) -> Option<u32> {
        offset(&self.labels, label)
    }

    /// Writes the field of `form` at `at` with `label`'s place plus `addend` when the label
    /// is bound ([`Buf::resolve`]); PC-relative forms take `S + A - P`. A field that may not
    /// reach the label is near ([`Buf::room`]).
    #[inline]
    pub fn fixup(&mut self, at: u32, form: Form, label: Label, addend: i64) {
        let fixup = Fixup { at, form, label, addend };
        let index = self.fixups.len() as u32;
        self.fixups.push(fixup);
        if let Some(reach) = self.tracked(form)
            && !reaches(&self.labels, &fixup)
        {
            let deadline =
                (i64::from(at) - addend + reach.ahead).clamp(0, i64::from(NEVER - 1)) as u32;
            self.near.push(Near { fixup: index, deadline });
            self.due = self.due.min(deadline);
        }
    }

    /// The reach of `form` when the code could grow past it, so a field of it may be out of
    /// reach.
    #[inline]
    fn tracked(&self, form: Form) -> Option<Reach> {
        let reach = form.reach()?;
        let least = reach.ahead.min(-reach.back) as u64;
        (reach.far != Far::Never && least < GROWTH * u64::from(self.bound)).then_some(reach)
    }

    /// Whether `label` is placed where the field of `form` at `at` can't reach it with
    /// `addend`: a use of a datum placed in an island behind it takes a new label.
    pub fn placed_out_of_reach(&self, at: u32, form: Form, label: Label, addend: i64) -> bool {
        self.offset(label).is_some() && !reaches(&self.labels, &Fixup { at, form, label, addend })
    }

    /// Makes room for `bytes` more bytes of code written as one piece (a stencil, the glue of
    /// an instruction, a relay, a trap stub): when writing them could take a near fixup past
    /// its deadline before the next piece, an island is written first ([module docs](self)),
    /// its data from `data` by label. A piece of stencil code makes at most one near fixup per
    /// 4 bytes, which the room counts. Refused when no island can serve what is due.
    #[inline]
    pub fn room<'d>(
        &mut self,
        bytes: u32,
        island: Option<&Island<'_>>,
        data: impl FnMut(Label) -> Option<&'d [u8]>,
    ) -> Result<(), Error> {
        if self.due == NEVER {
            return Ok(());
        }
        self.room_slow(bytes, island, data)
    }

    #[cold]
    #[inline(never)]
    fn room_slow<'d>(
        &mut self,
        bytes: u32,
        island: Option<&Island<'_>>,
        data: impl FnMut(Label) -> Option<&'d [u8]>,
    ) -> Result<(), Error> {
        let entry = u64::from(DATUM.max(island.map_or(0, |it| it.jump.len() as u32)));
        let ahead = u64::from(bytes) + (u64::from(bytes) / 4 + 1) * entry;
        if self.fits(ahead, island) {
            return Ok(());
        }
        self.prune();
        if self.fits(ahead, island) {
            return Ok(());
        }
        let Some(island) = island else {
            let first = self
                .near
                .iter()
                .min_by_key(|it| it.deadline)
                .map(|it| self.fixups[it.fixup as usize]);
            // alloc: the error's message, once per failed compile.
            return Err(Error::new(format!(
                "at {:#x}, {} fields are about to be out of reach ({}) and the library has no \
                 `jump` stencil to write an island of veneers with",
                self.len(),
                self.near.len(),
                first.map_or_else(String::new, |it| describe(&it)),
            )));
        };
        let start = self.len();
        self.island(ahead, island, data)?;
        if !self.fits(ahead, Some(island)) {
            // alloc: the error's message, once per failed compile.
            return Err(Error::new(format!(
                "the island at {start:#x} left {} fields that may be out of reach before \
                 {:#x}, the deadline of the first, with {bytes} bytes of code to write",
                self.near.len(),
                self.due,
            )));
        }
        Ok(())
    }

    /// Serves every near fixup still out of reach once the code is written: a branch back to
    /// a label behind its reach whose deadline the code ended before goes to a veneer in an
    /// island after the last piece. Refused, naming the first, when there is no island.
    pub fn settle<'d>(
        &mut self,
        island: Option<&Island<'_>>,
        data: impl FnMut(Label) -> Option<&'d [u8]>,
    ) -> Result<(), Error> {
        if self.due == NEVER {
            return Ok(());
        }
        self.prune();
        if self.near.is_empty() {
            return Ok(());
        }
        let Some(island) = island else {
            let first = self.fixups[self.near[0].fixup as usize];
            // alloc: the error's message, once per failed compile.
            return Err(Error::new(format!(
                "{} is out of reach, and the library has no `jump` stencil to write an island \
                 of veneers with",
                describe(&first)
            )));
        };
        self.island(u64::MAX, island, data)
    }

    /// The most bytes an island serving every near fixup takes.
    fn island_bound(&self, island: Option<&Island<'_>>) -> u64 {
        let jump = island.map_or(0, |it| it.jump.len() as u64);
        let align = island.map_or(0, |it| u64::from(it.align));
        let entry = u64::from(DATUM).max(jump);
        jump + align + self.near.len() as u64 * entry
    }

    /// Whether `ahead` more bytes and then an island serving every near fixup still come
    /// before the first deadline.
    fn fits(&self, ahead: u64, island: Option<&Island<'_>>) -> bool {
        u64::from(self.len()) + ahead + self.island_bound(island) <= u64::from(self.due)
    }

    /// Drops the near fixups whose labels are bound within their reach, and recounts the
    /// first deadline.
    fn prune(&mut self) {
        let (labels, fixups) = (&self.labels, &self.fixups);
        self.near.retain(|it| !reaches(labels, &fixups[it.fixup as usize]));
        self.due = self.near.iter().map(|it| it.deadline).min().unwrap_or(NEVER);
    }

    /// Writes an island serving each near fixup whose deadline would pass before `ahead` more
    /// bytes and the next island, and each past half its reach: a veneer per branch label, a
    /// placement per data label, the code before jumping over them.
    fn island<'d>(
        &mut self,
        ahead: u64,
        island: &Island<'_>,
        mut data: impl FnMut(Label) -> Option<&'d [u8]>,
    ) -> Result<(), Error> {
        let start = u64::from(self.len());
        let limit = start.saturating_add(ahead).saturating_add(2 * self.island_bound(Some(island)));
        self.served.clear();
        let (fixups, served) = (&self.fixups, &mut self.served);
        self.near.retain(|it| {
            let fixup = &fixups[it.fixup as usize];
            let half = fixup.form.reach().map_or(0, |reach| reach.ahead / 2) as u64;
            let deadline = u64::from(it.deadline);
            let serve = deadline <= limit || deadline.saturating_sub(start) <= half;
            if serve {
                served.push(it.fixup);
            }
            !serve
        });
        self.due = self.near.iter().map(|it| it.deadline).min().unwrap_or(NEVER);
        if self.served.is_empty() {
            return Ok(());
        }
        // Veneers first, then data, each by label, then as made: one veneer or placement a
        // label, in an order of the code's own.
        let temp = self.temp();
        let mut served = std::mem::replace(&mut self.served, Vec::new_in(temp));
        let fixups = &self.fixups;
        served.sort_unstable_by_key(|&it| {
            let fixup = &fixups[it as usize];
            (fixup.form.reach().map(|reach| reach.far as u8), fixup.label, it)
        });
        let over = self.label();
        self.jump(island, over);
        let mut first = 0;
        while first < served.len() {
            let fixup = self.fixups[served[first] as usize];
            let same = served[first..].iter().take_while(|&&it| {
                let it = &self.fixups[it as usize];
                (it.label, it.form.reach().map(|reach| reach.far))
                    == (fixup.label, fixup.form.reach().map(|reach| reach.far))
            });
            let end = first + same.count();
            match fixup.form.reach().map(|reach| reach.far) {
                Some(Far::Veneer) => self.veneer(island, &served[first..end])?,
                Some(Far::Place) => self.place(island, &fixup, &mut data)?,
                Some(Far::Never) | None => {
                    // alloc: the error's message, once per failed compile.
                    return Err(Error::new(format!(
                        "{}, and nothing stands in for its target",
                        describe(&fixup)
                    )));
                }
            }
            first = end;
        }
        self.bind(over);
        served.clear();
        self.served = served;
        Ok(())
    }

    /// The library's jump to `to`, here.
    fn jump(&mut self, island: &Island<'_>, to: Label) {
        let at = self.len();
        self.put(island.jump);
        self.fixup(at + island.at, island.form, to, island.addend);
    }

    /// A veneer of the branches `served` (to one label): bound here, jumping to the label,
    /// which they go to instead.
    fn veneer(&mut self, island: &Island<'_>, served: &[u32]) -> Result<(), Error> {
        let to = self.fixups[served[0] as usize].label;
        if let Some(fixup) =
            served.iter().map(|&it| self.fixups[it as usize]).find(|it| it.addend != 0)
        {
            // alloc: the error's message, once per failed compile.
            return Err(Error::new(format!(
                "{}: its addend is {}, so a veneer it went to would be entered off its start",
                describe(&fixup),
                fixup.addend
            )));
        }
        let veneer = self.label();
        self.bind(veneer);
        self.jump(island, to);
        for &it in served {
            self.fixups[it as usize].label = veneer;
        }
        Ok(())
    }

    /// The datum of `fixup`'s label, aligned and bound here.
    fn place<'d>(
        &mut self,
        island: &Island<'_>,
        fixup: &Fixup,
        data: &mut impl FnMut(Label) -> Option<&'d [u8]>,
    ) -> Result<(), Error> {
        if self.offset(fixup.label).is_some() {
            // alloc: the error's message, once per failed compile.
            return Err(Error::new(format!(
                "{}: its datum is placed already, out of its reach, and a label is placed once",
                describe(fixup)
            )));
        }
        let Some(bytes) = data(fixup.label).filter(|it| it.len() <= DATUM as usize) else {
            // alloc: the error's message, once per failed compile.
            return Err(Error::new(format!(
                "{}: no datum of at most {DATUM} bytes is known by its label to place within its reach",
                describe(fixup)
            )));
        };
        self.align(island.align, island.nop);
        self.bind(fixup.label);
        self.put(bytes);
        Ok(())
    }

    /// Writes the field of `form` at `at` with `value` now: a hole's immediate or offset.
    #[inline]
    pub fn patch(&mut self, at: u32, form: Form, value: i64) -> Result<(), enc::Overflow> {
        enc::write(form, &mut self.bytes, at as usize, value)
    }

    /// A relocation for the linker.
    #[inline]
    pub fn reloc(&mut self, at: u32, kind: binemit::Reloc, name: Name, addend: i64) {
        self.relocs.push(Reloc { at, kind, name, addend });
    }

    /// Writes every fixup, now that every label it names is bound. An unbound label or a
    /// value that does not fit its field is an error naming the form, the offset and the
    /// label's place.
    pub fn resolve(&mut self) -> Result<(), Error> {
        for fixup in &self.fixups {
            let Some(target) = offset(&self.labels, fixup.label) else {
                // alloc: the error's message, once per failed compile.
                return Err(Error::new(format!(
                    "the {:?} field at {:#x} names {:?}, which is never placed",
                    fixup.form, fixup.at, fixup.label
                )));
            };
            enc::write(fixup.form, &mut self.bytes, fixup.at as usize, value(fixup, target)).map_err(|e| {
                // alloc: the error's message, once per failed compile.
                Error::new(format!(
                    "the field at {:#x} to {:?}, placed at {target:#x}: {e}, in {} bytes of code \
                     no island brought it within reach of",
                    fixup.at,
                    fixup.label,
                    self.bytes.len()
                ))
            })?;
        }
        self.fixups.clear();
        self.near.clear();
        self.due = NEVER;
        Ok(())
    }

    /// The relocations, by offset once [`Buf::sort_relocs`] ran.
    pub fn relocs(&self) -> &[Reloc] {
        &self.relocs
    }

    /// Sorts the relocations by offset, keeping the order of those at one offset (what the
    /// stable `sort_by_key` did), in place.
    pub fn sort_relocs(&mut self) {
        sort_by_key_in_place(&mut self.relocs, |it| it.at);
    }
}

/// A stable sort that never allocates: an insertion sort, linear on what is already in order.
/// Relocations and call sites are recorded as the code is written, so they come nearly sorted.
pub(crate) fn sort_by_key_in_place<T: Copy, K: Ord>(items: &mut [T], key: impl Fn(&T) -> K) {
    if items.is_sorted_by_key(&key) {
        return;
    }
    for at in 1..items.len() {
        let item = items[at];
        let k = key(&item);
        let mut to = at;
        while to > 0 && key(&items[to - 1]) > k {
            items[to] = items[to - 1];
            to -= 1;
        }
        items[to] = item;
    }
}

fn offset(labels: &[u32], label: Label) -> Option<u32> {
    match labels.get(label.0 as usize) {
        Some(&UNBOUND) | None => None,
        Some(&at) => Some(at),
    }
}

/// The value of `fixup`'s field with its label at `target`.
fn value(fixup: &Fixup, target: u32) -> i64 {
    let value = i64::from(target) + fixup.addend;
    if fixup.form.pc_relative() { value - i64::from(fixup.at) } else { value }
}

/// Whether `fixup`'s label is bound and its field reaches it.
fn reaches(labels: &[u32], fixup: &Fixup) -> bool {
    offset(labels, fixup.label)
        .is_some_and(|target| enc::check(fixup.form, value(fixup, target)).is_ok())
}

/// A fixup, as an error names it.
fn describe(fixup: &Fixup) -> String {
    // alloc: the error's message, once per failed compile.
    format!("the {:?} field at {:#x} to {:?}", fixup.form, fixup.at, fixup.label)
}
