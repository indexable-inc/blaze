//! What a shim of blaze's reaches of the evaluator running a macro: its memory, checked as
//! the evaluator checks every access, and the running thread's `errno`. A shim stops the
//! expansion with a [`Stop`] when the macro does what it may not.

use std::fmt;

/// Why an expansion stopped that is not the macro's own panic: what it hit, said so that the
/// error names the macro after it ("the proc macro `x` <what>").
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Stop(pub(crate) String);

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The evaluator's memory and thread, as a shim reaches them (`run.rs` implements it over the
/// evaluator; a test, over a buffer).
pub(crate) trait Guest {
    /// `len` bytes at `addr`, as the program holds the address.
    fn read(&self, addr: u64, len: usize) -> Result<&[u8], Stop>;
    fn write(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Stop>;
    /// The running thread's `errno`.
    fn set_errno(&mut self, code: i32) -> Result<(), Stop>;
    /// The address of a copy of `bytes` the program never frees, aligned to `align`: what C's
    /// `getenv` gives back.
    fn alloc(&mut self, bytes: &[u8], align: usize) -> Result<u64, Stop>;

    /// The bytes of the NUL-terminated string at `addr`, without its NUL, of at most `max`.
    fn read_c_str(&self, addr: u64, max: usize) -> Result<&[u8], Stop> {
        const CHUNK: usize = 64;
        let mut len = 0;
        while len < max {
            let take = CHUNK.min(max - len);
            // A string that ends near the end of its block is read a byte at a time there.
            let chunk = match self.read(addr + len as u64, take) {
                Ok(chunk) => chunk,
                Err(_) if take > 1 => self.read(addr + len as u64, 1)?,
                Err(stop) => return Err(stop),
            };
            if let Some(at) = chunk.iter().position(|&b| b == 0) {
                return self.read(addr, len + at);
            }
            len += chunk.len();
        }
        // alloc: error path
        Err(Stop(format!("passed a string longer than {max} bytes, or one with no end")))
    }

    fn read_u64(&self, addr: u64) -> Result<u64, Stop> {
        let bytes = self.read(addr, 8)?;
        Ok(u64::from_le_bytes(bytes.try_into().expect("eight bytes were read")))
    }

    fn write_u64(&mut self, addr: u64, value: u64) -> Result<(), Stop> {
        self.write(addr, &value.to_le_bytes())
    }
}

/// The `n`th argument as a word, little-endian, of at most eight bytes.
pub(crate) fn word(args: &[&[u8]], n: usize) -> Result<u64, Stop> {
    let Some(arg) = args.get(n) else {
        // alloc: error path
        return Err(Stop(format!("called a shim with {} arguments, not {}", args.len(), n + 1)));
    };
    let mut bytes = [0; 8];
    let len = arg.len().min(8);
    bytes[..len].copy_from_slice(&arg[..len]);
    Ok(u64::from_le_bytes(bytes))
}

/// `value` written to `ret`, little-endian, truncated to its size.
pub(crate) fn put_word(ret: &mut [u8], value: u64) {
    let len = ret.len().min(8);
    ret[..len].copy_from_slice(&value.to_le_bytes()[..len]);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A guest over one buffer at address `base`, for the shims' tests.
    pub(crate) struct Buffer {
        pub(crate) base: u64,
        pub(crate) bytes: Vec<u8>,
        pub(crate) errno: Option<i32>,
    }

    impl Guest for Buffer {
        fn read(&self, addr: u64, len: usize) -> Result<&[u8], Stop> {
            let at = addr.checked_sub(self.base).map(|it| it as usize);
            match at.filter(|at| at + len <= self.bytes.len()) {
                Some(at) => Ok(&self.bytes[at..at + len]),
                None => Err(Stop(format!("read {len} bytes at {addr:#x}, outside its memory"))),
            }
        }

        fn write(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Stop> {
            let at = addr.checked_sub(self.base).map(|it| it as usize);
            match at.filter(|at| at + bytes.len() <= self.bytes.len()) {
                Some(at) => {
                    self.bytes[at..at + bytes.len()].copy_from_slice(bytes);
                    Ok(())
                }
                None => Err(Stop(format!("wrote {} bytes at {addr:#x}, outside its memory", bytes.len()))),
            }
        }

        fn set_errno(&mut self, code: i32) -> Result<(), Stop> {
            self.errno = Some(code);
            Ok(())
        }

        fn alloc(&mut self, bytes: &[u8], align: usize) -> Result<u64, Stop> {
            let at = self.bytes.len().next_multiple_of(align.max(1));
            self.bytes.resize(at, 0);
            self.bytes.extend_from_slice(bytes);
            Ok(self.base + at as u64)
        }
    }

    #[test]
    fn a_string_is_read_to_its_nul_and_one_without_is_refused() {
        let mut bytes = vec![b'x'; 100];
        bytes[70] = 0;
        let guest = Buffer { base: 0x1000, bytes, errno: None };
        assert_eq!(guest.read_c_str(0x1000, 1000).unwrap().len(), 70);
        assert!(guest.read_c_str(0x1000 + 71, 1000).is_err());
        assert!(guest.read_c_str(0x1000, 10).is_err());
    }
}
