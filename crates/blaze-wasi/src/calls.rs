//! The driver of the WASI calls: each one defined in a Wasmtime [`Linker`], its arguments taken
//! as Wasmtime passes them and handed in a straight line to the core (`layout`), which
//! reads and writes the guest's memory and asks the [`System`](crate::System).

use core::marker::PhantomData;

use wasmtime::{Caller, Linker};

use crate::{
    Embed, Open, Stop, System, Width,
    layout::{self, Halt, Mem, SUCCESS, answered},
};

/// The `len` bytes at `ptr` of `mem`, or why they are not in it.
///
/// Deprecated: [`bytes_at`](crate::bytes_at), whose refusal is the typed
/// [`Fault`](crate::Fault). Callers: blaze-proc-macro script.rs:226, 240, 261, 271, 272 and
/// host.rs:674, 704, 710.
pub fn bytes(mem: &[u8], ptr: u64, len: u64) -> Result<&[u8], Stop> {
    layout::bytes_at(mem, ptr, len).map_err(Stop::from)
}

/// Writes `bytes` at `ptr` of `mem`, or says why they don't fit.
///
/// Deprecated: [`put_at`](crate::put_at), whose refusal is the typed
/// [`Fault`](crate::Fault). Callers: blaze-proc-macro script.rs:233 and host.rs:663, 690.
pub fn put(mem: &mut [u8], ptr: u64, bytes: &[u8]) -> Result<(), Stop> {
    layout::put_at(mem, ptr, bytes).map_err(Stop::from)
}

/// Runs `f`, the call `name`, on the calling guest's memory, laid out at `W`, and its system.
fn call<W: Width, T: Embed, R>(
    caller: &mut Caller<'_, T>,
    name: &'static str,
    f: impl FnOnce(&mut Mem<'_, W>, &mut dyn System) -> Result<R, Halt>, // dyn: as `Embed::with`
) -> wasmtime::Result<R> {
    T::with(caller, |bytes, sys| {
        f(&mut Mem { bytes, call: name, width: PhantomData }, sys).map_err(Stop::from)
    })
}

/// Defines every call of [`FUNCTIONS`](crate::FUNCTIONS) in `l`, under `W`'s module, at `W`'s
/// width.
pub fn link<W: Width, T: Embed>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    let m = W::MODULE;
    l.func_wrap(m, "args_sizes_get", |mut c: Caller<'_, T>, count: W::Ptr, size: W::Ptr| {
        call::<W, T, _>(&mut c, "args_sizes_get", |mem, sys| {
            mem.sizes(sys.args()?, count.into(), size.into())
        })
    })?;
    l.func_wrap(m, "args_get", |mut c: Caller<'_, T>, argv: W::Ptr, buf: W::Ptr| {
        call::<W, T, _>(&mut c, "args_get", |mem, sys| {
            mem.strings(sys.args()?.0, argv.into(), buf.into())
        })
    })?;
    l.func_wrap(m, "environ_sizes_get", |mut c: Caller<'_, T>, count: W::Ptr, size: W::Ptr| {
        call::<W, T, _>(&mut c, "environ_sizes_get", |mem, sys| {
            mem.sizes(sys.environ()?, count.into(), size.into())
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
                layout::fd_write(mem, sys, fd, (iovs.into(), count.into()), written.into())
            })
        },
    )?;
    l.func_wrap(
        m,
        "fd_read",
        |mut c: Caller<'_, T>, fd: u32, iovs: W::Ptr, count: W::Ptr, read: W::Ptr| {
            call::<W, T, _>(&mut c, "fd_read", |mem, sys| {
                layout::fd_read(mem, sys, fd, (iovs.into(), count.into()), read.into())
            })
        },
    )?;
    l.func_wrap(m, "fd_prestat_get", |mut c: Caller<'_, T>, fd: u32, out: W::Ptr| {
        call::<W, T, _>(&mut c, "fd_prestat_get", |mem, _| mem.prestat(fd, out.into()))
    })?;
    l.func_wrap(
        m,
        "fd_prestat_dir_name",
        |mut c: Caller<'_, T>, fd: u32, path: W::Ptr, len: W::Ptr| {
            call::<W, T, _>(&mut c, "fd_prestat_dir_name", |mem, _| {
                mem.prestat_dir_name(fd, path.into(), len.into())
            })
        },
    )?;
    l.func_wrap(m, "fd_fdstat_get", |mut c: Caller<'_, T>, fd: u32, out: W::Ptr| {
        call::<W, T, _>(&mut c, "fd_fdstat_get", |mem, sys| {
            answered(sys.fd_filetype(fd)?, |filetype| mem.put_fdstat(out.into(), filetype))
        })
    })?;
    l.func_wrap(m, "fd_fdstat_set_flags", |mut c: Caller<'_, T>, fd: u32, flags: u32| {
        call::<W, T, _>(&mut c, "fd_fdstat_set_flags", |_, sys| {
            answered(sys.set_fd_flags(fd, flags)?, |()| Ok::<_, Halt>(()))
        })
    })?;
    l.func_wrap(m, "fd_close", |mut c: Caller<'_, T>, fd: u32| {
        call::<W, T, _>(&mut c, "fd_close", |_, sys| {
            answered(sys.close(fd)?, |()| Ok::<_, Halt>(()))
        })
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
                layout::fd_readdir(mem, sys, fd, (buf.into(), len.into()), cookie, out.into())
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
                answered(target, |target| mem.put_link(&target, (buf.into(), buf_len.into()), out.into()))
            })
        },
    )?;
    l.func_wrap(
        m,
        "poll_oneoff",
        |mut c: Caller<'_, T>, _in: W::Ptr, _out: W::Ptr, _n: W::Ptr, _events: W::Ptr| {
            call::<W, T, _>(&mut c, "poll_oneoff", |_, sys| Err::<i32, _>(Halt::Stop(sys.poll_oneoff())))
        },
    )?;
    l.func_wrap(m, "proc_exit", |mut c: Caller<'_, T>, code: u32| {
        call::<W, T, _>(&mut c, "proc_exit", |_, sys| Err::<(), _>(Halt::Stop(sys.exit(code))))
    })?;
    l.func_wrap(m, "sched_yield", |_: Caller<'_, T>| SUCCESS)?;
    Ok(())
}
