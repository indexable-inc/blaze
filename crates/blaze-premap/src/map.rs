//! The image in memory: one [`CodeMemory`] per key on the machine, holding the image's
//! address space from its base (gaps and the zeroed tails of segments are pages never
//! written, which take no memory), and the [`FixedMap`]s every run maps it by, each at its
//! linked address inside the workspace executable's reservation for fixed maps (plus the
//! run's slide): code read and execute, read-only data read only, both shared; data copied on
//! a run's first write, so each run gets the image's initial values of its statics and none
//! sees another's writes.

use blaze_inproc::{Access, CodeMemory, CodeRange, FixedMap};

use crate::PremapError;
use crate::image::View;
use crate::prelink::Prot;

/// An image's memory object, filled.
pub struct Memory {
    memory: CodeMemory,
    base: u64,
    maps: Vec<(CodeRange, u64, Access)>,
}

impl Memory {
    /// The memory object of `view`, written and published once.
    pub fn of(view: &View<'_>) -> Result<Memory, PremapError> {
        let meta = view.meta();
        if !view.whole() {
            return Err(PremapError::of(format_args!("the dependency image {:?} is read without its segment bytes", meta.key.0)));
        }
        let len = view.end() - meta.base;
        let mut name = String::from("blaze-premap-"); // alloc: setup, once per image: its memory object's name
        blaze_format::hex::encode_into(&meta.key.0.0[..8], &mut name);
        let mut memory = CodeMemory::new(&name, len, 0)?;
        let page = memory.page();
        if meta.base % page != 0 {
            return Err(PremapError::of(format_args!(
                "the dependency image's base {:#x} is not a multiple of this machine's mapping granule {page:#x}",
                meta.base
            )));
        }
        let mut maps = Vec::with_capacity(view.segments().count());
        for segment in view.segments() {
            if segment.addr % page != 0 {
                return Err(PremapError::of(format_args!(
                    "the dependency image's segment at {:#x} is not a multiple of this machine's mapping \
                     granule {page:#x}",
                    segment.addr
                )));
            }
            let offset = segment.addr - meta.base;
            memory.write(offset, segment.bytes)?;
            let range = CodeRange { offset, len: segment.size.next_multiple_of(page) };
            let access = match segment.prot {
                Prot::Code => {
                    memory.publish(CodeRange { offset, len: segment.size })?;
                    Access::Execute
                }
                Prot::ReadOnly => Access::Read,
                Prot::Data => Access::Copy,
            };
            maps.push((range, segment.addr, access));
        }
        Ok(Memory { memory, base: meta.base, maps })
    }

    /// What a run maps, each at its fixed address.
    pub fn maps(&self) -> impl Iterator<Item = FixedMap<'_>> + '_ {
        self.maps.iter().map(|&(range, at, access)| FixedMap { memory: &self.memory, range, at, access })
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    /// Bytes of the object: the image's address space.
    pub fn len(&self) -> u64 {
        self.memory.len()
    }

    pub fn is_empty(&self) -> bool {
        self.memory.is_empty()
    }

    /// The bytes the object holds at `range`, for tests and checks.
    pub fn bytes(&self, range: CodeRange) -> Result<&[u8], PremapError> {
        Ok(self.memory.bytes(range)?)
    }
}