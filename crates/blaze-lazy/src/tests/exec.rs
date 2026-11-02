//! Executable memory for the tests that run the stubs on this machine: blaze-inproc's
//! [`Executable`], written once.

use blaze_inproc::exec::Executable;

pub(crate) struct Code {
    code: Executable,
    len: usize,
}

impl Code {
    /// `bytes`, executable.
    pub(crate) fn new(bytes: &[u8]) -> Code {
        let code = Executable::new(bytes, 0).unwrap_or_else(|it| panic!("executable memory for the stubs: {}", it.what));
        Code { code, len: bytes.len() }
    }

    /// The address of the byte at `offset`.
    pub(crate) fn address(&self, offset: u32) -> u64 {
        assert!((offset as usize) < self.len);
        self.code.code_address() + u64::from(offset)
    }
}
