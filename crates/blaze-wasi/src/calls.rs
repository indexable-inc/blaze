//! Each call's arguments, read from and written to the guest's memory at its [`Width`], and the
//! [`System`] asked in between.

use std::{marker::PhantomData, ops::Range};

use wasmtime::{Caller, Linker};

use crate::{Dirent, Embed, Errno, Filestat, Open, ROOT, ROOT_FD, Stop, System, Width};

/// `errno`'s success.
const SUCCESS: i32 = 0;

/// The bytes `len` long at `ptr` of a memory of `size` bytes, or why they are not in it.
fn range(size: usize, ptr: u64, len: u64) -> Result<Range<usize>, Stop> {
    match ptr.checked_add(len) {
        Some(end) if end <= size as u64 => Ok(ptr as usize..end as usize),
        _ => Err(Stop(format!(
            // alloc: error path
            "passed the host {len} bytes at {ptr:#x}, outside its memory of {size} bytes"
        ))),
    }
}

/// The `len` bytes at `ptr` of `mem`, or why they are not in it.
pub fn bytes(mem: &[u8], ptr: u64, len: u64) -> Result<&[u8], Stop> {
    Ok(&mem[range(mem.len(), ptr, len)?])
}

/// Writes `bytes` at `ptr` of `mem`, or says why they don't fit.
pub fn put(mem: &mut [u8], ptr: u64, bytes: &[u8]) -> Result<(), Stop> {
    let at = range(mem.len(), ptr, bytes.len() as u64)?;
    mem[at].copy_from_slice(bytes);
    Ok(())
}

/// `ptr + by`, or why it is past every memory.
fn offset(ptr: u64, by: u64) -> Result<u64, Stop> {
    // alloc: error path
    ptr.checked_add(by)
        .ok_or_else(|| Stop(format!("passed the host an address past 2^64 ({ptr:#x} + {by})")))
}

/// A guest's memory, as a guest of width `W` lays its arguments out, in the call `call`, which
/// every error about an address names.
pub(crate) struct Mem<'m, W> {
    pub(crate) bytes: &'m mut [u8],
    pub(crate) call: &'static str,
    pub(crate) width: PhantomData<W>,
}

impl<W: Width> Mem<'_, W> {
    /// `stop`, saying which call it was in.
    pub(crate) fn named(&self, stop: &Stop) -> Stop {
        Stop(format!("{stop}, in `{}`", self.call)) // alloc: error path
    }

    /// `ptr + by`, or why it is past every memory.
    fn at(&self, ptr: u64, by: u64) -> Result<u64, Stop> {
        offset(ptr, by).map_err(|stop| self.named(&stop))
    }

    pub(crate) fn get(&self, ptr: u64, len: u64) -> Result<&[u8], Stop> {
        bytes(self.bytes, ptr, len).map_err(|stop| self.named(&stop))
    }

    pub(crate) fn get_mut(&mut self, ptr: u64, len: u64) -> Result<&mut [u8], Stop> {
        let at = range(self.bytes.len(), ptr, len).map_err(|stop| self.named(&stop))?;
        Ok(&mut self.bytes[at])
    }

    pub(crate) fn put(&mut self, ptr: u64, bytes: &[u8]) -> Result<(), Stop> {
        put(self.bytes, ptr, bytes).map_err(|stop| self.named(&stop))
    }

    fn put_u32(&mut self, ptr: u64, value: u32) -> Result<(), Stop> {
        self.put(ptr, &value.to_le_bytes())
    }

    pub(crate) fn put_u64(&mut self, ptr: u64, value: u64) -> Result<(), Stop> {
        self.put(ptr, &value.to_le_bytes())
    }

    /// A `size` or a pointer, in the guest's [`Width::BYTES`].
    pub(crate) fn put_size(&mut self, ptr: u64, value: u64) -> Result<(), Stop> {
        let bytes = value.to_le_bytes();
        if W::BYTES < 8 && value >> (8 * W::BYTES) != 0 {
            // alloc: error path
            return Err(self.named(&Stop(format!(
                "was to be given the size {value}, past its {}-bit pointers",
                8 * W::BYTES
            ))));
        }
        self.put(ptr, &bytes[..W::BYTES as usize])
    }

    fn read_size(&self, ptr: u64) -> Result<u64, Stop> {
        let mut word = [0; 8];
        word[..W::BYTES as usize].copy_from_slice(self.get(ptr, W::BYTES)?);
        Ok(u64::from_le_bytes(word))
    }

    /// The `n`th `iovec` of the array at `iovs`: its buffer's address and length.
    fn iovec(&self, iovs: u64, n: u64) -> Result<(u64, u64), Stop> {
        let at = self.at(iovs, n.saturating_mul(2 * W::BYTES))?;
        Ok((self.read_size(at)?, self.read_size(self.at(at, W::BYTES)?)?))
    }

    /// `strings` (each NUL-terminated) at `buf`, and a pointer to each at `pointers`: what
    /// `args_get` and `environ_get` write.
    fn strings(&mut self, strings: &[u8], pointers: u64, buf: u64) -> Result<i32, Stop> {
        self.put(buf, strings)?;
        let mut start = 0u64;
        for (n, (end, _)) in strings.iter().enumerate().filter(|(_, b)| **b == 0).enumerate() {
            self.put_size(self.at(pointers, n as u64 * W::BYTES)?, self.at(buf, start)?)?;
            start = end as u64 + 1;
        }
        Ok(SUCCESS)
    }

    /// WASI's `filestat` of `stat` at `out`.
    fn put_filestat(&mut self, out: u64, stat: &Filestat) -> Result<(), Stop> {
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
}

/// The `errno` of `answer`, with `ok` run on its value first.
pub(crate) fn answered<T>(
    answer: Result<T, Errno>,
    ok: impl FnOnce(T) -> Result<(), Stop>,
) -> Result<i32, Stop> {
    match answer {
        Ok(value) => {
            ok(value)?;
            Ok(SUCCESS)
        }
        Err(errno) => Ok(i32::from(errno.0)),
    }
}

/// Runs `f`, the call `name`, on the calling guest's memory, laid out at `W`, and its system.
fn call<W: Width, T: Embed, R>(
    caller: &mut Caller<'_, T>,
    name: &'static str,
    f: impl FnOnce(&mut Mem<'_, W>, &mut dyn System) -> Result<R, Stop>, // dyn: as `Embed::with`
) -> wasmtime::Result<R> {
    T::with(caller, |bytes, sys| f(&mut Mem { bytes, call: name, width: PhantomData }, sys))
}

/// Defines every call of [`FUNCTIONS`](crate::FUNCTIONS) in `l`, under `W`'s module, at `W`'s
/// width.
pub fn link<W: Width, T: Embed>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    let m = W::MODULE;
    l.func_wrap(m, "args_sizes_get", |mut c: Caller<'_, T>, count: W::Ptr, size: W::Ptr| {
        call::<W, T, _>(&mut c, "args_sizes_get", |mem, sys| {
            let (strings, n) = sys.args()?;
            let len = strings.len() as u64;
            mem.put_size(count.into(), n as u64)?;
            mem.put_size(size.into(), len)?;
            Ok(SUCCESS)
        })
    })?;
    l.func_wrap(m, "args_get", |mut c: Caller<'_, T>, argv: W::Ptr, buf: W::Ptr| {
        call::<W, T, _>(&mut c, "args_get", |mem, sys| {
            mem.strings(sys.args()?.0, argv.into(), buf.into())
        })
    })?;
    l.func_wrap(m, "environ_sizes_get", |mut c: Caller<'_, T>, count: W::Ptr, size: W::Ptr| {
        call::<W, T, _>(&mut c, "environ_sizes_get", |mem, sys| {
            let (strings, n) = sys.environ()?;
            let len = strings.len() as u64;
            mem.put_size(count.into(), n as u64)?;
            mem.put_size(size.into(), len)?;
            Ok(SUCCESS)
        })
    })?;
    l.func_wrap(m, "environ_get", |mut c: Caller<'_, T>, pointers: W::Ptr, buf: W::Ptr| {
        call::<W, T, _>(&mut c, "environ_get", |mem, sys| {
            mem.strings(sys.environ()?.0, pointers.into(), buf.into())
        })
    })?;
    l.func_wrap(m, "clock_res_get", |mut c: Caller<'_, T>, id: u32, out: W::Ptr| {
        call::<W, T, _>(&mut c, "clock_res_get", |mem, sys| {
            answered(sys.clock_res(id)?, |ns| mem.put_u64(out.into(), ns))
        })
    })?;
    l.func_wrap(
        m,
        "clock_time_get",
        |mut c: Caller<'_, T>, id: u32, _precision: u64, out: W::Ptr| {
            // A store that counts no fuel says so with an error: then there is none to give.
            let fuel_left = c.get_fuel().ok();
            call::<W, T, _>(&mut c, "clock_time_get", |mem, sys| {
                answered(sys.clock_time(id, fuel_left)?, |ns| mem.put_u64(out.into(), ns))
            })
        },
    )?;
    l.func_wrap(m, "random_get", |mut c: Caller<'_, T>, buf: W::Ptr, len: W::Ptr| {
        call::<W, T, _>(&mut c, "random_get", |mem, sys| {
            sys.random(mem.get_mut(buf.into(), len.into())?)?;
            Ok(SUCCESS)
        })
    })?;
    l.func_wrap(
        m,
        "fd_write",
        |mut c: Caller<'_, T>, fd: u32, iovs: W::Ptr, count: W::Ptr, written: W::Ptr| {
            call::<W, T, _>(&mut c, "fd_write", |mem, sys| {
                fd_write(mem, sys, fd, (iovs.into(), count.into()), written.into())
            })
        },
    )?;
    l.func_wrap(
        m,
        "fd_read",
        |mut c: Caller<'_, T>, fd: u32, iovs: W::Ptr, count: W::Ptr, read: W::Ptr| {
            call::<W, T, _>(&mut c, "fd_read", |mem, sys| {
                fd_read(mem, sys, fd, (iovs.into(), count.into()), read.into())
            })
        },
    )?;
    l.func_wrap(m, "fd_prestat_get", |mut c: Caller<'_, T>, fd: u32, out: W::Ptr| {
        call::<W, T, _>(&mut c, "fd_prestat_get", |mem, _| {
            if fd != ROOT_FD {
                return Ok(i32::from(Errno::BADF.0));
            }
            let out = out.into();
            // The tag `dir` (0) and its padding, then `pr_name_len`, a size.
            mem.put(out, &[0; 8][..W::BYTES as usize])?;
            mem.put_size(mem.at(out, W::BYTES)?, ROOT.len() as u64)?;
            Ok(SUCCESS)
        })
    })?;
    l.func_wrap(
        m,
        "fd_prestat_dir_name",
        |mut c: Caller<'_, T>, fd: u32, path: W::Ptr, len: W::Ptr| {
            call::<W, T, _>(&mut c, "fd_prestat_dir_name", |mem, _| {
                if fd != ROOT_FD {
                    return Ok(i32::from(Errno::BADF.0));
                }
                let len: u64 = len.into();
                if len < ROOT.len() as u64 {
                    return Ok(i32::from(Errno::INVAL.0));
                }
                mem.put(path.into(), ROOT)?;
                Ok(SUCCESS)
            })
        },
    )?;
    l.func_wrap(m, "fd_fdstat_get", |mut c: Caller<'_, T>, fd: u32, out: W::Ptr| {
        call::<W, T, _>(&mut c, "fd_fdstat_get", |mem, sys| {
            answered(sys.fd_filetype(fd)?, |filetype| {
                // No flags; every right, base and inheriting.
                let mut stat = [0; 24];
                stat[0] = filetype as u8;
                stat[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
                stat[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
                mem.put(out.into(), &stat)
            })
        })
    })?;
    l.func_wrap(m, "fd_fdstat_set_flags", |mut c: Caller<'_, T>, fd: u32, flags: u32| {
        call::<W, T, _>(&mut c, "fd_fdstat_set_flags", |_, sys| {
            answered(sys.set_fd_flags(fd, flags)?, |()| Ok(()))
        })
    })?;
    l.func_wrap(m, "fd_close", |mut c: Caller<'_, T>, fd: u32| {
        call::<W, T, _>(&mut c, "fd_close", |_, sys| answered(sys.close(fd)?, |()| Ok(())))
    })?;
    l.func_wrap(
        m,
        "fd_seek",
        |mut c: Caller<'_, T>, fd: u32, by: i64, whence: u32, out: W::Ptr| {
            call::<W, T, _>(&mut c, "fd_seek", |mem, sys| {
                answered(sys.seek(fd, by, whence)?, |at| mem.put_u64(out.into(), at))
            })
        },
    )?;
    // `fd_tell` is a seek by 0 from where it is.
    l.func_wrap(m, "fd_tell", |mut c: Caller<'_, T>, fd: u32, out: W::Ptr| {
        call::<W, T, _>(&mut c, "fd_tell", |mem, sys| {
            answered(sys.seek(fd, 0, 1)?, |at| mem.put_u64(out.into(), at))
        })
    })?;
    l.func_wrap(m, "fd_filestat_get", |mut c: Caller<'_, T>, fd: u32, out: W::Ptr| {
        call::<W, T, _>(&mut c, "fd_filestat_get", |mem, sys| {
            answered(sys.fd_stat(fd)?, |stat| mem.put_filestat(out.into(), &stat))
        })
    })?;
    l.func_wrap(
        m,
        "fd_readdir",
        |mut c: Caller<'_, T>, fd: u32, buf: W::Ptr, len: W::Ptr, cookie: u64, out: W::Ptr| {
            call::<W, T, _>(&mut c, "fd_readdir", |mem, sys| {
                fd_readdir(mem, sys, fd, (buf.into(), len.into()), cookie, out.into())
            })
        },
    )?;
    l.func_wrap(
        m,
        "path_open",
        |mut c: Caller<'_, T>,
         fd: u32,
         dirflags: u32,
         path: W::Ptr,
         len: W::Ptr,
         oflags: u32,
         rights_base: u64,
         rights_inheriting: u64,
         fdflags: u32,
         out: W::Ptr| {
            let how = Open { dirflags, oflags, rights_base, rights_inheriting, fdflags };
            call::<W, T, _>(&mut c, "path_open", |mem, sys| {
                let opened = sys.open(fd, mem.get(path.into(), len.into())?, how)?;
                answered(opened, |new| mem.put_u32(out.into(), new))
            })
        },
    )?;
    l.func_wrap(
        m,
        "path_filestat_get",
        |mut c: Caller<'_, T>, fd: u32, flags: u32, path: W::Ptr, len: W::Ptr, out: W::Ptr| {
            call::<W, T, _>(&mut c, "path_filestat_get", |mem, sys| {
                let stat = sys.path_stat(fd, mem.get(path.into(), len.into())?, flags & 1 != 0)?;
                answered(stat, |stat| mem.put_filestat(out.into(), &stat))
            })
        },
    )?;
    l.func_wrap(
        m,
        "path_readlink",
        |mut c: Caller<'_, T>,
         fd: u32,
         path: W::Ptr,
         len: W::Ptr,
         buf: W::Ptr,
         buf_len: W::Ptr,
         out: W::Ptr| {
            call::<W, T, _>(&mut c, "path_readlink", |mem, sys| {
                let target = sys.readlink(fd, mem.get(path.into(), len.into())?)?;
                answered(target, |target| {
                    // Cut to the buffer, as `readlink` does.
                    let buf_len: u64 = buf_len.into();
                    let n = target.len().min(usize::try_from(buf_len).unwrap_or(usize::MAX));
                    mem.put(buf.into(), &target[..n])?;
                    mem.put_size(out.into(), n as u64)
                })
            })
        },
    )?;
    l.func_wrap(
        m,
        "poll_oneoff",
        |mut c: Caller<'_, T>, _in: W::Ptr, _out: W::Ptr, _n: W::Ptr, _events: W::Ptr| {
            call::<W, T, _>(&mut c, "poll_oneoff", |_, sys| Err::<i32, _>(sys.poll_oneoff()))
        },
    )?;
    l.func_wrap(m, "proc_exit", |mut c: Caller<'_, T>, code: u32| {
        call::<W, T, _>(&mut c, "proc_exit", |_, sys| Err::<(), _>(sys.exit(code)))
    })?;
    l.func_wrap(m, "sched_yield", |_: Caller<'_, T>| SUCCESS)?;
    Ok(())
}

/// `fd_write(fd, iovs, iovs_len, written)`: each buffer to the system in turn, until one is
/// written short. An error after some bytes were written ends the call with those counted.
fn fd_write<W: Width>(
    mem: &mut Mem<'_, W>,
    sys: &mut dyn System,
    fd: u32,
    (iovs, count): (u64, u64),
    written: u64,
) -> Result<i32, Stop> {
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
fn fd_read<W: Width>(
    mem: &mut Mem<'_, W>,
    sys: &mut dyn System,
    fd: u32,
    (iovs, count): (u64, u64),
    read: u64,
) -> Result<i32, Stop> {
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
fn fd_readdir<W: Width>(
    mem: &mut Mem<'_, W>,
    sys: &mut dyn System,
    fd: u32,
    (buf, buf_len): (u64, u64),
    cookie: u64,
    out: u64,
) -> Result<i32, Stop> {
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
