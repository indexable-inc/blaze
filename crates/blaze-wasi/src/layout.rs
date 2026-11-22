//! The core: each WASI call's arguments read from and its answer written to a guest's memory, at
//! the guest's pointer width, with the [`System`] asked in between. Pure functions over a slice
//! of the guest's memory and a system handed in: no Wasmtime, no `std`, no allocation, no read of
//! anything but what it is handed. The driver ([`crate::link`], [`crate::seed::link`])
//! takes each guest call to one function here in a straight line.
//!
//! A refusal of this crate's own is a [`Fault`] (an address or a size the guest passed that is
//! not in its memory or not in its width), carried as [`Halt::Fault`] with the call it was in;
//! what the system says stops the guest is [`Halt::Stop`], as it said it.

#![blaze::sans_io]

use core::{fmt, marker::PhantomData, ops::Range};

use crate::{Dirent, Errno, Filestat, Filetype, ROOT, ROOT_FD, Stop, System};

/// The width of a guest's pointers and sizes, as its memory lays them out: what the core needs
/// of a [`crate::Width`] (which adds the argument type Wasmtime passes and the import module).
pub trait Pointers {
    /// The bytes of a pointer or a `size` in the guest's memory: 4 or 8.
    const BYTES: u64;
}

/// An address or a size a guest passed that the host refuses: the guest is stopped with it,
/// never a host fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// `len` bytes at `ptr` are not all in a memory of `size` bytes.
    OutsideMemory { ptr: u64, len: u64, size: usize },
    /// `ptr + by` is past 2^64.
    PastAddressSpace { ptr: u64, by: u64 },
    /// A size the host was to write back, `value`, does not fit the guest's `bits`-bit pointers.
    SizePastWidth { value: u64, bits: u64 },
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Fault::OutsideMemory { ptr, len, size } => write!(
                f,
                "passed the host {len} bytes at {ptr:#x}, outside its memory of {size} bytes"
            ),
            Fault::PastAddressSpace { ptr, by } => {
                write!(f, "passed the host an address past 2^64 ({ptr:#x} + {by})")
            }
            Fault::SizePastWidth { value, bits } => {
                write!(f, "was to be given the size {value}, past its {bits}-bit pointers")
            }
        }
    }
}

/// Why a call ends the guest: a [`Fault`] of this crate's in the call `call`, or what the
/// system said.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Halt {
    Fault { call: &'static str, fault: Fault },
    Stop(Stop),
}

impl From<Stop> for Halt {
    fn from(stop: Stop) -> Halt {
        Halt::Stop(stop)
    }
}

impl fmt::Display for Halt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Halt::Fault { call, fault } => write!(f, "{fault}, in `{call}`"),
            Halt::Stop(stop) => fmt::Display::fmt(stop, f),
        }
    }
}

/// `errno`'s success.
pub(crate) const SUCCESS: i32 = 0;

/// The bytes `len` long at `ptr` of a memory of `size` bytes, or why they are not in it.
fn range(size: usize, ptr: u64, len: u64) -> Result<Range<usize>, Fault> {
    match ptr.checked_add(len) {
        Some(end) if end <= size as u64 => Ok(ptr as usize..end as usize),
        _ => Err(Fault::OutsideMemory { ptr, len, size }),
    }
}

/// The `len` bytes at `ptr` of `mem`, or why they are not in it.
pub fn bytes_at(mem: &[u8], ptr: u64, len: u64) -> Result<&[u8], Fault> {
    Ok(&mem[range(mem.len(), ptr, len)?])
}

/// Writes `bytes` at `ptr` of `mem`, or says why they don't fit.
pub fn put_at(mem: &mut [u8], ptr: u64, bytes: &[u8]) -> Result<(), Fault> {
    let at = range(mem.len(), ptr, bytes.len() as u64)?;
    mem[at].copy_from_slice(bytes);
    Ok(())
}

/// `ptr + by`, or why it is past every memory.
fn offset(ptr: u64, by: u64) -> Result<u64, Fault> {
    ptr.checked_add(by).ok_or(Fault::PastAddressSpace { ptr, by })
}

/// A guest's memory, as a guest of width `W` lays its arguments out, in the call `call`, which
/// every fault about an address names.
pub(crate) struct Mem<'m, W> {
    pub(crate) bytes: &'m mut [u8],
    pub(crate) call: &'static str,
    pub(crate) width: PhantomData<W>,
}

impl<W: Pointers> Mem<'_, W> {
    /// `fault`, in this call.
    fn fault(&self, fault: Fault) -> Halt {
        Halt::Fault { call: self.call, fault }
    }

    /// `ptr + by`, or why it is past every memory.
    fn at(&self, ptr: u64, by: u64) -> Result<u64, Halt> {
        offset(ptr, by).map_err(|fault| self.fault(fault))
    }

    pub(crate) fn get(&self, ptr: u64, len: u64) -> Result<&[u8], Halt> {
        bytes_at(self.bytes, ptr, len).map_err(|fault| self.fault(fault))
    }

    pub(crate) fn get_mut(&mut self, ptr: u64, len: u64) -> Result<&mut [u8], Halt> {
        let at = range(self.bytes.len(), ptr, len).map_err(|fault| self.fault(fault))?;
        Ok(&mut self.bytes[at])
    }

    pub(crate) fn put(&mut self, ptr: u64, bytes: &[u8]) -> Result<(), Halt> {
        let call = self.call;
        put_at(self.bytes, ptr, bytes).map_err(|fault| Halt::Fault { call, fault })
    }

    pub(crate) fn put_u32(&mut self, ptr: u64, value: u32) -> Result<(), Halt> {
        self.put(ptr, &value.to_le_bytes())
    }

    pub(crate) fn put_u64(&mut self, ptr: u64, value: u64) -> Result<(), Halt> {
        self.put(ptr, &value.to_le_bytes())
    }

    /// A `size` or a pointer, in the guest's [`Pointers::BYTES`].
    pub(crate) fn put_size(&mut self, ptr: u64, value: u64) -> Result<(), Halt> {
        let bytes = value.to_le_bytes();
        if W::BYTES < 8 && value >> (8 * W::BYTES) != 0 {
            return Err(self.fault(Fault::SizePastWidth { value, bits: 8 * W::BYTES }));
        }
        self.put(ptr, &bytes[..W::BYTES as usize])
    }

    fn read_size(&self, ptr: u64) -> Result<u64, Halt> {
        let mut word = [0; 8];
        word[..W::BYTES as usize].copy_from_slice(self.get(ptr, W::BYTES)?);
        Ok(u64::from_le_bytes(word))
    }

    /// The `n`th `iovec` of the array at `iovs`: its buffer's address and length.
    fn iovec(&self, iovs: u64, n: u64) -> Result<(u64, u64), Halt> {
        let at = self.at(iovs, n.saturating_mul(2 * W::BYTES))?;
        Ok((self.read_size(at)?, self.read_size(self.at(at, W::BYTES)?)?))
    }

    /// `args_sizes_get` and `environ_sizes_get`: how many `strings` there are (`n`) at `count`
    /// and their bytes at `size`.
    pub(crate) fn sizes(&mut self, (strings, n): (&[u8], usize), count: u64, size: u64) -> Result<i32, Halt> {
        let len = strings.len() as u64;
        self.put_size(count, n as u64)?;
        self.put_size(size, len)?;
        Ok(SUCCESS)
    }

    /// `strings` (each NUL-terminated) at `buf`, and a pointer to each at `pointers`: what
    /// `args_get` and `environ_get` write.
    pub(crate) fn strings(&mut self, strings: &[u8], pointers: u64, buf: u64) -> Result<i32, Halt> {
        self.put(buf, strings)?;
        let mut start = 0u64;
        for (n, (end, _)) in strings.iter().enumerate().filter(|(_, b)| **b == 0).enumerate() {
            self.put_size(self.at(pointers, n as u64 * W::BYTES)?, self.at(buf, start)?)?;
            start = end as u64 + 1;
        }
        Ok(SUCCESS)
    }

    /// `fd_prestat_get(fd, out)`: the one preopened directory's `prestat`.
    pub(crate) fn prestat(&mut self, fd: u32, out: u64) -> Result<i32, Halt> {
        if fd != ROOT_FD {
            return Ok(i32::from(Errno::BADF.0));
        }
        // The tag `dir` (0) and its padding, then `pr_name_len`, a size.
        self.put(out, &[0; 8][..W::BYTES as usize])?;
        self.put_size(self.at(out, W::BYTES)?, ROOT.len() as u64)?;
        Ok(SUCCESS)
    }

    /// `fd_prestat_dir_name(fd, path, len)`: the one preopened directory's name.
    pub(crate) fn prestat_dir_name(&mut self, fd: u32, path: u64, len: u64) -> Result<i32, Halt> {
        if fd != ROOT_FD {
            return Ok(i32::from(Errno::BADF.0));
        }
        if len < ROOT.len() as u64 {
            return Ok(i32::from(Errno::INVAL.0));
        }
        self.put(path, ROOT)?;
        Ok(SUCCESS)
    }

    /// WASI's `fdstat` of a descriptor of `filetype` at `out`: no flags; every right, base and
    /// inheriting.
    pub(crate) fn put_fdstat(&mut self, out: u64, filetype: Filetype) -> Result<(), Halt> {
        let mut stat = [0; 24];
        stat[0] = filetype as u8;
        stat[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        stat[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        self.put(out, &stat)
    }

    /// WASI's `filestat` of `stat` at `out`.
    pub(crate) fn put_filestat(&mut self, out: u64, stat: &Filestat) -> Result<(), Halt> {
        let mut bytes = [0; 64];
        bytes[0..8].copy_from_slice(&stat.dev.to_le_bytes());
        bytes[8..16].copy_from_slice(&stat.ino.to_le_bytes());
        bytes[16] = stat.filetype as u8;
        bytes[24..32].copy_from_slice(&stat.nlink.to_le_bytes());
        bytes[32..40].copy_from_slice(&stat.size.to_le_bytes());
        bytes[40..48].copy_from_slice(&stat.atim.to_le_bytes());
        bytes[48..56].copy_from_slice(&stat.mtim.to_le_bytes());
        bytes[56..64].copy_from_slice(&stat.ctim.to_le_bytes());
        self.put(out, &bytes)
    }

    /// `path_readlink`'s answer: `target` cut to the buffer `(buf, buf_len)`, as `readlink`
    /// does, and how much of it was written at `out`.
    pub(crate) fn put_link(&mut self, target: &[u8], (buf, buf_len): (u64, u64), out: u64) -> Result<(), Halt> {
        let n = target.len().min(usize::try_from(buf_len).unwrap_or(usize::MAX));
        self.put(buf, &target[..n])?;
        self.put_size(out, n as u64)
    }
}

/// The `errno` of `answer`, with `ok` run on its value first.
pub(crate) fn answered<T, E>(
    answer: Result<T, Errno>,
    ok: impl FnOnce(T) -> Result<(), E>,
) -> Result<i32, E> {
    match answer {
        Ok(value) => {
            ok(value)?;
            Ok(SUCCESS)
        }
        Err(errno) => Ok(i32::from(errno.0)),
    }
}

/// `fd_write(fd, iovs, iovs_len, written)`: each buffer to the system in turn, until one is
/// written short. An error after some bytes were written ends the call with those counted.
pub(crate) fn fd_write<W: Pointers>(
    mem: &mut Mem<'_, W>,
    sys: &mut dyn System,
    fd: u32,
    (iovs, count): (u64, u64),
    written: u64,
) -> Result<i32, Halt> {
    // No buffers still asks the system, so a bad descriptor is refused as with some.
    if count == 0
        && let Err(errno) = sys.write(fd, &[])?
    {
        return Ok(i32::from(errno.0));
    }
    let mut total = 0u64;
    for n in 0..count {
        let (base, len) = mem.iovec(iovs, n)?;
        match sys.write(fd, mem.get(base, len)?)? {
            Ok(wrote) => {
                total += wrote as u64;
                if (wrote as u64) < len {
                    break;
                }
            }
            Err(errno) if total == 0 => return Ok(i32::from(errno.0)),
            Err(_) => break,
        }
    }
    mem.put_size(written, total)?;
    Ok(SUCCESS)
}

/// `fd_read(fd, iovs, iovs_len, read)`: each buffer filled in turn, until one is filled short.
pub(crate) fn fd_read<W: Pointers>(
    mem: &mut Mem<'_, W>,
    sys: &mut dyn System,
    fd: u32,
    (iovs, count): (u64, u64),
    read: u64,
) -> Result<i32, Halt> {
    // As in `fd_write`: no buffers still asks the system about the descriptor.
    if count == 0
        && let Err(errno) = sys.read(fd, &mut [])?
    {
        return Ok(i32::from(errno.0));
    }
    let mut total = 0u64;
    for n in 0..count {
        let (base, len) = mem.iovec(iovs, n)?;
        match sys.read(fd, mem.get_mut(base, len)?)? {
            Ok(got) => {
                total += got as u64;
                if (got as u64) < len {
                    break;
                }
            }
            Err(errno) if total == 0 => return Ok(i32::from(errno.0)),
            Err(_) => break,
        }
    }
    mem.put_size(read, total)?;
    Ok(SUCCESS)
}

/// `fd_readdir(fd, buf, buf_len, cookie, out)`: the entries of a directory from entry `cookie`
/// on, each a 24-byte `dirent` and its name, as many as fit; the last one cut short, and `out`
/// then `buf_len`, when they don't.
pub(crate) fn fd_readdir<W: Pointers>(
    mem: &mut Mem<'_, W>,
    sys: &mut dyn System,
    fd: u32,
    (buf, buf_len): (u64, u64),
    cookie: u64,
    out: u64,
) -> Result<i32, Halt> {
    // No room still asks the system, so a bad descriptor or a refused directory is refused as
    // with room.
    if buf_len == 0
        && let Err(errno) = sys.dir_entry(fd, cookie)?
    {
        return Ok(i32::from(errno.0));
    }
    let mut used = 0u64;
    let mut n = cookie;
    while used < buf_len {
        let Dirent { ino, filetype, name } = match sys.dir_entry(fd, n)? {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(errno) => return Ok(i32::from(errno.0)),
        };
        let mut head = [0u8; 24];
        head[0..8].copy_from_slice(&n.wrapping_add(1).to_le_bytes());
        head[8..16].copy_from_slice(&ino.to_le_bytes());
        head[16..20].copy_from_slice(&(name.len() as u32).to_le_bytes());
        head[20] = filetype as u8;
        for part in [&head[..], name] {
            let room = buf_len - used;
            let part = &part[..part.len().min(usize::try_from(room).unwrap_or(usize::MAX))];
            mem.put(mem.at(buf, used)?, part)?;
            used += part.len() as u64;
        }
        n = n.wrapping_add(1);
    }
    mem.put_size(out, used)?;
    Ok(SUCCESS)
}
