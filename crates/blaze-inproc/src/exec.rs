//! Code written once, then run in this process, with data beside it: what a test that runs
//! machine code it made (`blaze-lazy`'s stubs, `blaze-stencil`'s linked units) maps it into.
//! The memory is private to the process; [`crate::CodeMemory`] is the one shared with a run.
//!
//! The code is mapped writable, written, then made read-and-execute (Linux and macOS
//! `mprotect`; Windows `VirtualProtectEx`), with the instruction cache made coherent by the
//! same maintenance as [`crate::CodeMemory::publish`]. It is never writable again, and no
//! page is ever writable and executable at once: no per-thread switch (macOS `MAP_JIT`) is
//! involved, so a program's own JIT can't change it and a forked child runs it as it is.
//! On macOS the pages are unsigned, so a process that enforces code signing (the hardened
//! runtime without `allow-unsigned-executable-memory`) is refused. Unmapped when dropped.

use crate::{Error, sys};

#[cfg(test)]
mod tests;

pub struct Executable {
    memory: sys::Exec,
}

impl Executable {
    /// `code`, executable, and `data_len` zeroed writable bytes after it.
    pub fn new(code: &[u8], data_len: usize) -> Result<Executable, Error> {
        Executable::written(code.len(), data_len, |out, _| out.copy_from_slice(code))
    }

    /// `code_len` bytes of code and `data_len` zeroed writable bytes after them, the data
    /// within 1 GiB of every code byte (in PC-relative reach both ways), written by `write`:
    /// for code whose bytes depend on where it and its data are (a linker's relocations).
    ///
    /// `write` is given the code and the data, writable, at the addresses they run at, on the
    /// calling thread; when it returns the code is made executable. Other `Executable`s run
    /// meanwhile, on this thread too.
    pub fn written(
        code_len: usize,
        data_len: usize,
        write: impl FnOnce(&mut [u8], &mut [u8]),
    ) -> Result<Executable, Error> {
        Ok(Executable { memory: sys::Exec::new(code_len, data_len, write)? })
    }

    /// Where the code's first byte is.
    pub fn code_address(&self) -> u64 {
        self.memory.code_address()
    }

    /// Where the data's first byte is.
    pub fn data_address(&self) -> u64 {
        self.memory.data_address()
    }

    pub fn data(&mut self) -> &mut [u8] {
        self.memory.data()
    }
}
