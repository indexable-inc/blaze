//! The one buffer a function is written into: its bytes, labels for places not written yet
//! (blocks, edges, trap stubs, constants), fixups resolved once every label is bound, and the
//! relocations the linker resolves. Reserved once per function from the sizes of the
//! stencils and the glue's bounds, on the compile's frame: writing never allocates.

use blaze_mem::frame::Frame;
use cranelift_codegen::binemit;

use crate::{
    Error,
    code::{Name, Reloc},
    enc::{self, Form},
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

const UNBOUND: u32 = u32::MAX;

pub struct Buf<'f> {
    bytes: Vec<u8, &'f Frame>,
    /// By label: its offset, or `u32::MAX` while unbound.
    labels: Vec<u32, &'f Frame>,
    fixups: Vec<Fixup, &'f Frame>,
    relocs: Vec<Reloc, &'f Frame>,
}

impl<'f> Buf<'f> {
    /// An empty buffer on `frame`, the compile's.
    pub fn new_in(frame: &'f Frame) -> Buf<'f> {
        Buf {
            bytes: Vec::new_in(frame),
            labels: Vec::new_in(frame),
            fixups: Vec::new_in(frame),
            relocs: Vec::new_in(frame),
        }
    }

    /// The frame the buffer is on, for the compile's other temporaries.
    pub fn temp(&self) -> &'f Frame {
        *self.bytes.allocator()
    }

    /// Room for `bytes` more bytes, `labels` labels and `fixups` fixups: one reservation per
    /// function, from the sizes the patcher adds up.
    pub fn reserve(&mut self, bytes: usize, labels: usize, fixups: usize) {
        self.bytes.reserve(bytes);
        self.labels.reserve(labels);
        self.fixups.reserve(fixups);
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
        match self.labels.get(label.0 as usize) {
            Some(&UNBOUND) | None => None,
            Some(&at) => Some(at),
        }
    }

    /// Writes the field of `form` at `at` with `label`'s place plus `addend` when the label
    /// is bound ([`Buf::resolve`]); PC-relative forms take `S + A - P`.
    #[inline]
    pub fn fixup(&mut self, at: u32, form: Form, label: Label, addend: i64) {
        self.fixups.push(Fixup { at, form, label, addend });
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
    /// value that does not fit its field is an error naming the form and the offset.
    pub fn resolve(&mut self) -> Result<(), Error> {
        for fixup in &self.fixups {
            let target = match self.labels.get(fixup.label.0 as usize) {
                Some(&UNBOUND) | None => {
                    // alloc: the error's message, once per failed compile.
                    return Err(Error::new(format!(
                        "the {:?} field at {:#x} names {:?}, which is never placed",
                        fixup.form, fixup.at, fixup.label
                    )));
                }
                Some(&target) => target,
            };
            let mut value = i64::from(target) + fixup.addend;
            if fixup.form.pc_relative() {
                value -= i64::from(fixup.at);
            }
            enc::write(fixup.form, &mut self.bytes, fixup.at as usize, value).map_err(|e| {
                // alloc: the error's message, once per failed compile.
                Error::new(format!("the field at {:#x} to {:?}: {e}", fixup.at, fixup.label))
            })?;
        }
        self.fixups.clear();
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
