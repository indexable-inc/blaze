//! Windows, through ntdll and kernel32.
//!
//! The stable interface is the DLLs' exports, declared directly (`#[link(name = "ntdll")]`,
//! `#[link(name = "kernel32")]`), with no `windows-sys`. Slice 1 specifies the table
//! (sys.md, "Windows") and builds no call: each operation is an [`crate::Unbuilt`] naming
//! itself ([`UNBUILT`]), and blaze-core's IOCP driver stays on the ban list.
//!
//! | need | import |
//! |---|---|
//! | open beneath a handle | `NtCreateFile` with a `RootDirectory` handle |
//! | read, write at an offset | `NtReadFile`, `NtWriteFile` (overlapped, on the port) |
//! | sync | `NtFlushBuffersFileEx` (`FLUSH_FLAGS_FILE_DATA_SYNC_ONLY` for a barrier) |
//! | rename, remove | `NtSetInformationFile` (`FileRenameInformationEx`, `FileDispositionInformationEx`: POSIX semantics) |
//! | clone | `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS, Dev Drive) |
//! | memory | `NtAllocateVirtualMemory`, `NtFreeVirtualMemory`, `NtProtectVirtualMemory` |
//! | park, unpark | `RtlWaitOnAddress`, `RtlWakeAddressSingle` (ntdll; `WaitOnAddress` is their kernel32 forwarder) |
//! | completions | `NtCreateIoCompletion`, `NtRemoveIoCompletionEx`, `NtSetIoCompletion` |
//! | a child's exit | `NtWaitForSingleObject` on the process handle, by a wait packet on the port (`NtAssociateWaitCompletionPacket`) |
//! | clock | `RtlQueryPerformanceCounter`, `NtQuerySystemTimePrecise` |
//! | random | `ProcessPrng` (bcryptprimitives) |
//! | sockets | `\Device\Afd` by `NtDeviceIoControlFile`, as the IOCP driver already does |

crate::unbuilt! {
    "Windows":
    open_beneath, close, read_at, write_at, sync_barrier, sync_full, set_len, rename, remove,
    make_dir, stat, list, clone_file, reserve, commit, release, protect, park, unpark,
    completion_port, child_exit, clock, sleep_until, spawn, exit, random, socket, connect, bind,
    listen, accept, send, receive, shutdown, pipe,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unbuilt_operation_names_itself() {
        let refusal = park();
        assert_eq!((refusal.operation, refusal.target), ("park", "Windows"));
        assert!(UNBUILT.contains(&"park"));
        assert!(refusal.to_string().contains("`park` on Windows"));
    }
}
