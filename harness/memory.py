"""memory.py: how much memory a process tree holds, the same way on Linux, macOS and Windows.

The one helper the benchmarks measure memory with (bench.py and the rest); a
harness that measures memory imports this rather than reading /proc or `ps` itself. What it
measures, and how far off it can be, is docs/benchmarks.md, "Memory and disk". In short:

- `run` starts a command and waits for it with `wait4`. The kernel keeps the peak resident
  set of every process of the command's tree it waited for, and hands back the largest one
  (`ru_maxrss`): exact, whatever the processes' lifetimes. `largest_mb` in the result is that,
  or the largest peak the sampler saw, whichever is more.
- What the tree holds at once (`peak_mb`) the kernel does not keep, so a `Sampler` reads what
  every process of the trees it watches holds every `INTERVAL` seconds and keeps the largest
  sum. A process watched once stays watched while it lives, so a daemon the command started
  is followed after the command ends and the daemon is reparented. A peak shorter than the
  interval, or a process that lives less than it, can be missed; the sum is never below
  `largest_mb`.
- `resident_mb` is what a tree holds now (a long-lived daemon's steady state), `disk_mb` what
  a directory takes on disk.

What a process holds: on Linux its resident set (VmRSS, `/proc/PID/statm`), the number
`ru_maxrss` keeps the peak of. On macOS its physical footprint (`proc_pid_rusage`), what
Activity Monitor shows: the memory it owns, counted when macOS has compressed or swapped it
out too, which it does to an idle daemon's pages when memory is short, and without the pages of
shared libraries every rustc maps; the kernel keeps each process's peak of it, which the
sampler reads as that process's largest (for a process that was already running when the
sampler started, such as a daemon, only a peak above the one it had then).

On Windows a process holds its private commit (`PrivateUsage`, `GetProcessMemoryInfo`): the
memory it owns, whether in RAM or written to the page file, without the shared pages of the
DLLs every process maps, so the number macOS's footprint is, and the one a job object's memory
limit enforces. The kernel keeps each process's peak of it (`PeakPagefileUsage`), which the
sampler reads as the process's largest. `run` starts the command suspended in a job object of
its own and lets it go once it is in the job, so everything it starts is in the job too (the
daemon included: blaze starts it detached from the console, not out of the job); when the
command ends the job says the most one process of it committed (`PeakProcessMemoryUsed`, the
`largest_mb`) and the most all of them committed at once (`PeakJobMemoryUsed`): both exact, so
on Windows `peak_mb` is not sampled. The job also says the CPU time its processes used
(`cpu_s`), which on Linux and macOS is the CPU time `wait4` reports. Python 3 standard library
only.
"""
import ctypes, os, platform, stat, subprocess, sys, tempfile, threading, time

import counters

DARWIN = platform.system() == "Darwin"
WINDOWS = platform.system() == "Windows"
if not WINDOWS:
    import resource
INTERVAL = float(os.environ.get("BLAZE_MEMORY_INTERVAL", "0.01"))
PAGE_KB = os.sysconf("SC_PAGE_SIZE") // 1024 if hasattr(os, "sysconf") else 4

if DARWIN:
    _proc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)

if WINDOWS:
    from ctypes import wintypes as _w

    _k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _psapi = ctypes.WinDLL("psapi", use_last_error=True)
    _ntdll = ctypes.WinDLL("ntdll")
    _PROCESS_QUERY_LIMITED_INFORMATION, _PROCESS_SET_QUOTA, _PROCESS_TERMINATE = 0x1000, 0x0100, 0x0001
    _PROCESS_SUSPEND_RESUME, _STILL_ACTIVE, _TH32CS_SNAPPROCESS = 0x0800, 259, 0x2
    _CREATE_SUSPENDED = 0x4
    _INVALID_HANDLE = ctypes.c_void_p(-1).value

    class _MemoryCounters(ctypes.Structure):
        # PROCESS_MEMORY_COUNTERS_EX (psapi.h).
        _fields_ = [("cb", _w.DWORD), ("PageFaultCount", _w.DWORD), ("PeakWorkingSetSize", ctypes.c_size_t),
                    ("WorkingSetSize", ctypes.c_size_t), ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                    ("QuotaPagedPoolUsage", ctypes.c_size_t), ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                    ("QuotaNonPagedPoolUsage", ctypes.c_size_t), ("PagefileUsage", ctypes.c_size_t),
                    ("PeakPagefileUsage", ctypes.c_size_t), ("PrivateUsage", ctypes.c_size_t)]

    class _ProcessEntry(ctypes.Structure):
        # PROCESSENTRY32W (tlhelp32.h).
        _fields_ = [("dwSize", _w.DWORD), ("cntUsage", _w.DWORD), ("th32ProcessID", _w.DWORD),
                    ("th32DefaultHeapID", ctypes.c_size_t), ("th32ModuleID", _w.DWORD), ("cntThreads", _w.DWORD),
                    ("th32ParentProcessID", _w.DWORD), ("pcPriClassBase", _w.LONG), ("dwFlags", _w.DWORD),
                    ("szExeFile", _w.WCHAR * 260)]

    class _IoCounters(ctypes.Structure):
        _fields_ = [(name, ctypes.c_uint64) for name in
                    ("Reads", "Writes", "Others", "ReadBytes", "WriteBytes", "OtherBytes")]

    class _BasicLimits(ctypes.Structure):
        # JOBOBJECT_BASIC_LIMIT_INFORMATION (winnt.h).
        _fields_ = [("PerProcessUserTimeLimit", ctypes.c_int64), ("PerJobUserTimeLimit", ctypes.c_int64),
                    ("LimitFlags", _w.DWORD), ("MinimumWorkingSetSize", ctypes.c_size_t),
                    ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", _w.DWORD),
                    ("Affinity", ctypes.c_size_t), ("PriorityClass", _w.DWORD), ("SchedulingClass", _w.DWORD)]

    class _ExtendedLimits(ctypes.Structure):
        # JOBOBJECT_EXTENDED_LIMIT_INFORMATION, information class 9.
        _fields_ = [("Basic", _BasicLimits), ("Io", _IoCounters), ("ProcessMemoryLimit", ctypes.c_size_t),
                    ("JobMemoryLimit", ctypes.c_size_t), ("PeakProcessMemoryUsed", ctypes.c_size_t),
                    ("PeakJobMemoryUsed", ctypes.c_size_t)]

    class _Accounting(ctypes.Structure):
        # JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, information class 1; times in 100 ns.
        _fields_ = [("TotalUserTime", ctypes.c_int64), ("TotalKernelTime", ctypes.c_int64),
                    ("ThisPeriodTotalUserTime", ctypes.c_int64), ("ThisPeriodTotalKernelTime", ctypes.c_int64),
                    ("TotalPageFaultCount", _w.DWORD), ("TotalProcesses", _w.DWORD),
                    ("ActiveProcesses", _w.DWORD), ("TotalTerminatedProcesses", _w.DWORD)]

    class _MemoryStatus(ctypes.Structure):
        # MEMORYSTATUSEX (sysinfoapi.h).
        _fields_ = [("dwLength", _w.DWORD), ("dwMemoryLoad", _w.DWORD), ("ullTotalPhys", ctypes.c_uint64),
                    ("ullAvailPhys", ctypes.c_uint64), ("ullTotalPageFile", ctypes.c_uint64),
                    ("ullAvailPageFile", ctypes.c_uint64), ("ullTotalVirtual", ctypes.c_uint64),
                    ("ullAvailVirtual", ctypes.c_uint64), ("ullAvailExtendedVirtual", ctypes.c_uint64)]

    _k32.OpenProcess.restype = _w.HANDLE
    _k32.OpenProcess.argtypes = [_w.DWORD, _w.BOOL, _w.DWORD]
    _k32.CloseHandle.argtypes = [_w.HANDLE]
    _k32.GetExitCodeProcess.argtypes = [_w.HANDLE, ctypes.POINTER(_w.DWORD)]
    _k32.GetProcessTimes.argtypes = [_w.HANDLE] + [ctypes.POINTER(ctypes.c_uint64)] * 4
    _k32.CreateToolhelp32Snapshot.restype = _w.HANDLE
    _k32.CreateToolhelp32Snapshot.argtypes = [_w.DWORD, _w.DWORD]
    _k32.Process32FirstW.argtypes = [_w.HANDLE, ctypes.POINTER(_ProcessEntry)]
    _k32.Process32NextW.argtypes = [_w.HANDLE, ctypes.POINTER(_ProcessEntry)]
    _k32.CreateJobObjectW.restype = _w.HANDLE
    _k32.CreateJobObjectW.argtypes = [ctypes.c_void_p, _w.LPCWSTR]
    _k32.AssignProcessToJobObject.argtypes = [_w.HANDLE, _w.HANDLE]
    _k32.QueryInformationJobObject.argtypes = [_w.HANDLE, ctypes.c_int, ctypes.c_void_p, _w.DWORD, ctypes.c_void_p]
    _k32.IsProcessInJob.argtypes = [_w.HANDLE, _w.HANDLE, ctypes.POINTER(_w.BOOL)]
    _k32.GlobalMemoryStatusEx.argtypes = [ctypes.POINTER(_MemoryStatus)]
    _psapi.GetProcessMemoryInfo.argtypes = [_w.HANDLE, ctypes.POINTER(_MemoryCounters), _w.DWORD]
    # Undocumented, in ntdll since NT and what Process Explorer and Sysinternals' tools resume a
    # whole process with: subprocess keeps no handle of the suspended main thread to resume it by.
    _ntdll.NtResumeProcess.argtypes = [_w.HANDLE]
    _ntdll.NtResumeProcess.restype = ctypes.c_long
    # Undocumented, for the page files' use (SystemPageFileInformation, class 18), which no
    # documented call says: GlobalMemoryStatusEx's page file is the commit limit, RAM included.
    _ntdll.NtQuerySystemInformation.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong)]
    _ntdll.NtQuerySystemInformation.restype = ctypes.c_long

    def _win_error(what):
        code = ctypes.get_last_error()
        return OSError(code, f"{what}: {ctypes.FormatError(code).strip()}")

    def _opened(pid, access=_PROCESS_QUERY_LIMITED_INFORMATION):
        """A handle of `pid` that the caller closes, or None when it is gone or not ours to open."""
        return _k32.OpenProcess(access, False, pid) or None

    def _alive(handle):
        code = _w.DWORD()
        return bool(_k32.GetExitCodeProcess(handle, ctypes.byref(code))) and code.value == _STILL_ACTIVE

    def _started(handle):
        """When the process of `handle` started, in 100 ns since 1601."""
        created, ended, kernel, user = (ctypes.c_uint64() for _ in range(4))
        if not _k32.GetProcessTimes(handle, *(ctypes.byref(t) for t in (created, ended, kernel, user))):
            return None
        return created.value

    def _processes():
        """Every process of the machine now, as {pid: (parent pid, name)}."""
        snap = _k32.CreateToolhelp32Snapshot(_TH32CS_SNAPPROCESS, 0)
        if snap == _INVALID_HANDLE:
            raise _win_error("CreateToolhelp32Snapshot")
        found = {}
        try:
            entry = _ProcessEntry(dwSize=ctypes.sizeof(_ProcessEntry))
            ok = _k32.Process32FirstW(snap, ctypes.byref(entry))
            while ok:
                found[entry.th32ProcessID] = (entry.th32ParentProcessID, entry.szExeFile)
                ok = _k32.Process32NextW(snap, ctypes.byref(entry))
        finally:
            _k32.CloseHandle(snap)
        return found

    def _below(root, processes):
        """`root` and every process below it in `processes` (`_processes()`). Windows keeps a
        parent's pid after the parent ends and hands the pid out again, so a process counts as
        a child only when it started after its parent did."""
        by_parent = {}
        for pid, (parent, _) in processes.items():
            if pid != parent:
                by_parent.setdefault(parent, []).append(pid)
        started = {}

        def start_of(pid):
            if pid not in started:
                handle = _opened(pid)
                started[pid] = _started(handle) if handle else None
                if handle:
                    _k32.CloseHandle(handle)
            return started[pid]

        found, todo = [], [root]
        while todo:
            pid = todo.pop()
            found.append(pid)
            for child in by_parent.get(pid, ()):
                mine, theirs = start_of(pid), start_of(child)
                if mine is not None and theirs is not None and theirs >= mine:
                    todo.append(child)
        return found

    class Job:
        """A job object that what `start` starts runs in, with all it starts: its peaks of
        committed memory and the CPU time of its processes, exact, read when asked."""

        def __init__(self):
            self.handle = _k32.CreateJobObjectW(None, None)
            if not self.handle:
                raise _win_error("CreateJobObjectW")

        def start(self, argv, popen=subprocess.Popen, **kwargs):
            """`popen(argv, **kwargs)`, suspended until it is in the job."""
            kwargs["creationflags"] = kwargs.get("creationflags", 0) | _CREATE_SUSPENDED
            child = popen(argv, **kwargs)
            handle = int(child._handle)  # noqa: SLF001 - subprocess's own handle of the process
            if not _k32.AssignProcessToJobObject(self.handle, handle):
                error = _win_error(f"AssignProcessToJobObject of {argv[0]}")
                child.kill()
                child.wait()
                raise error
            status = _ntdll.NtResumeProcess(handle)
            if status != 0:
                child.kill()
                child.wait()
                raise OSError(f"NtResumeProcess of {argv[0]}: NTSTATUS {status & 0xFFFFFFFF:#010x}")
            return child

        def peaks_mb(self):
            """(the most one process committed, the most all committed at once), in megabytes."""
            info = _ExtendedLimits()
            if not _k32.QueryInformationJobObject(self.handle, 9, ctypes.byref(info), ctypes.sizeof(info), None):
                raise _win_error("QueryInformationJobObject(JobObjectExtendedLimitInformation)")
            return round(info.PeakProcessMemoryUsed / 2**20), round(info.PeakJobMemoryUsed / 2**20)

        def _accounting(self):
            info = _Accounting()
            if not _k32.QueryInformationJobObject(self.handle, 1, ctypes.byref(info), ctypes.sizeof(info), None):
                raise _win_error("QueryInformationJobObject(JobObjectBasicAccountingInformation)")
            return info

        def cpu_s(self):
            """The CPU time, user and kernel, every process of the job has used so far, ended
            ones included."""
            info = self._accounting()
            return (info.TotalUserTime + info.TotalKernelTime) / 1e7

        def active(self):
            """How many processes of the job run now."""
            return self._accounting().ActiveProcesses

        def holds(self, handle):
            """Whether the process of `handle` is in the job."""
            inside = _w.BOOL()
            if not _k32.IsProcessInJob(handle, self.handle, ctypes.byref(inside)):
                raise _win_error("IsProcessInJob")
            return bool(inside)

        def close(self):
            # No KILL_ON_JOB_CLOSE: a daemon started in the job lives on after it.
            if self.handle:
                _k32.CloseHandle(self.handle)
                self.handle = None


def running(pid: int) -> bool:
    """Whether a process `pid` runs now (a zombie, not reaped, counts on Unix). The one way a
    harness asks that on Windows, where `os.kill(pid, 0)` is TerminateProcess with exit code 0:
    it ends the process it was asking about."""
    if WINDOWS:
        handle = _opened(pid)
        if handle is None:
            # Gone, or there and not ours to open (ERROR_ACCESS_DENIED).
            return ctypes.get_last_error() == 5
        try:
            return _alive(handle)
        finally:
            _k32.CloseHandle(handle)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        pass
    return True


def maxrss_mb(usage):
    """`ru_maxrss` of a `resource.struct_rusage` in megabytes: Linux counts it in KiB, macOS in bytes."""
    return round(usage.ru_maxrss / (2**20 if DARWIN else 2**10))


def held_kb(pid):
    """What `pid` holds now and the most it has held, in KiB, or None when it is gone (or a
    zombie, which holds nothing). On Linux the most is what it holds now: the sampler keeps
    the peak."""
    if WINDOWS:
        handle = _opened(pid)
        if handle is None:
            return None
        try:
            counters_ = _MemoryCounters(cb=ctypes.sizeof(_MemoryCounters))
            if not _alive(handle) or not _psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters_), counters_.cb):
                return None
        finally:
            _k32.CloseHandle(handle)
        return counters_.PrivateUsage // 1024, max(counters_.PeakPagefileUsage, counters_.PrivateUsage) // 1024
    if DARWIN:
        # rusage_info_v4, read where it is read for instructions too (counters.py).
        info = counters.rusage(pid)
        if info is None or not info["footprint"]:
            return None
        return info["footprint"] // 1024, info["lifetime_max_footprint"] // 1024
    try:
        with open(f"/proc/{pid}/statm") as f:
            pages = int(f.read().split()[1])
    except (OSError, ValueError, IndexError):
        return None
    return (pages * PAGE_KB, pages * PAGE_KB) if pages else None


def children(pid):
    """The processes `pid` started that still run."""
    if WINDOWS:
        return _below(pid, _processes())[1:]
    if DARWIN:
        size = 256
        while True:
            buf = (ctypes.c_int * size)()
            n = _proc.proc_listchildpids(pid, buf, ctypes.sizeof(buf))
            if n <= 0:
                return []
            # It counts processes on some releases and bytes on others.
            n = n // ctypes.sizeof(ctypes.c_int) if n > size else n
            if n < size:
                return [p for p in buf[:n] if p > 0]
            size *= 4
    found = []
    try:
        tasks = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return []
    # A child's parent is the thread that started it: every thread has a list.
    for task in tasks:
        try:
            with open(f"/proc/{pid}/task/{task}/children") as f:
                found += [int(p) for p in f.read().split()]
        except OSError:
            pass
    return found


def tree(root):
    """`root` and every process below it that still runs."""
    if WINDOWS:
        # One snapshot of the machine for the whole tree, not one for each process of it.
        return _below(root, _processes())
    found, todo = [], [root]
    while todo:
        pid = todo.pop()
        found.append(pid)
        todo += children(pid)
    return found


def system_mb():
    """What the whole machine has and uses now, in megabytes: `total`, `used` (all but what
    the kernel could hand out without swapping, `MemAvailable`), `swap_total` and `swap_used`
    (on Windows the page files'). None on macOS."""
    if WINDOWS:
        status = _MemoryStatus(dwLength=ctypes.sizeof(_MemoryStatus))
        if not _k32.GlobalMemoryStatusEx(ctypes.byref(status)):
            raise _win_error("GlobalMemoryStatusEx")
        pages_total = pages_used = 0
        size, needed = 4096, ctypes.c_ulong()
        while True:
            buf = ctypes.create_string_buffer(size)
            code = _ntdll.NtQuerySystemInformation(18, buf, size, ctypes.byref(needed)) & 0xFFFFFFFF
            if code != 0xC0000004:  # STATUS_INFO_LENGTH_MISMATCH: a larger buffer
                break
            size = max(size * 2, needed.value)
        if code != 0:
            raise OSError(f"NtQuerySystemInformation(SystemPageFileInformation): NTSTATUS {code:#010x}")
        # SYSTEM_PAGEFILE_INFORMATION, one a page file: NextEntryOffset, TotalSize, TotalInUse
        # (both in pages), PeakUsage, then the file's name.
        at = 0
        while needed.value:
            following, total, in_use = (int.from_bytes(buf.raw[at + 4 * i:at + 4 * i + 4], "little") for i in range(3))
            pages_total, pages_used = pages_total + total, pages_used + in_use
            if not following:
                break
            at += following
        mb = lambda n: round(n / 2**20)
        return dict(total=mb(status.ullTotalPhys), used=mb(status.ullTotalPhys - status.ullAvailPhys),
                    swap_total=mb(pages_total * 4096), swap_used=mb(pages_used * 4096))
    try:
        with open("/proc/meminfo") as f:
            info = {line.split(":")[0]: int(line.split()[1]) for line in f if line.split()[1:]}
    except (OSError, ValueError):
        return None
    mb = lambda kb: round(kb / 1024)
    return dict(
        total=mb(info["MemTotal"]), used=mb(info["MemTotal"] - info["MemAvailable"]),
        swap_total=mb(info["SwapTotal"]), swap_used=mb(info["SwapTotal"] - info["SwapFree"]),
    )


def largest_processes(count):
    """The `count` processes of the whole machine that hold the most, as (megabytes resident,
    megabytes swapped out, pid, name), most first. Linux only: empty elsewhere."""
    found = []
    for pid in (int(p) for p in os.listdir("/proc") if p.isdigit()) if os.path.isdir("/proc") else ():
        try:
            with open(f"/proc/{pid}/status") as f:
                status = dict(line.rstrip("\n").split(":\t", 1) for line in f if ":\t" in line)
        except OSError:
            continue
        kb = lambda field: int(status.get(field, "0 kB").split()[0])
        if kb("VmRSS") or kb("VmSwap"):
            found.append((round(kb("VmRSS") / 1024), round(kb("VmSwap") / 1024), pid, status.get("Name", "?").strip()))
    return sorted(found, key=lambda it: it[0] + it[1], reverse=True)[:count]


def cpu_seconds(pid):
    """The CPU time `pid` has used, user and system, in seconds; None when it is gone."""
    if WINDOWS:
        handle = _opened(pid)
        if handle is None:
            return None
        try:
            created, ended, kernel, user = (ctypes.c_uint64() for _ in range(4))
            if not _k32.GetProcessTimes(handle, *(ctypes.byref(t) for t in (created, ended, kernel, user))):
                return None
        finally:
            _k32.CloseHandle(handle)
        return (kernel.value + user.value) / 1e7
    if DARWIN:
        got = counters.rusage(pid)
        return got["cpu_ns"] / 1e9 if got else None
    try:
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()
    except (OSError, IndexError):
        return None
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


# Windows: the jobs `run` left a process running in (a daemon), each still open.
_KEPT_JOBS = []


def tree_cpu_seconds(pid):
    """The CPU time, user and system, `pid` and every process it started have used, ended ones
    included; None when it is gone or its tree's time cannot be read. On Linux and macOS that is
    the process's own and its reaped children's (a daemon reaps what it runs: build scripts,
    proc macros, the linker). On Windows, which keeps no such total of a process, it is the time
    of the job `run` started `pid` in, whose processes are `pid`'s tree and the command that
    started it: a `pid` that `run` did not start has none, and None."""
    if WINDOWS:
        handle = _opened(pid)
        if handle is None:
            return None
        try:
            for job in list(_KEPT_JOBS):
                if not job.active():
                    _KEPT_JOBS.remove(job)
                    job.close()
                elif job.holds(handle):
                    return job.cpu_s()
        finally:
            _k32.CloseHandle(handle)
        return None
    if DARWIN:
        got = counters.rusage(pid)
        return got["tree_cpu_ns"] / 1e9 if got else None
    try:
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()
    except (OSError, IndexError):
        return None
    # utime, stime, cutime, cstime (proc(5)).
    return sum(int(f) for f in fields[11:15]) / os.sysconf("SC_CLK_TCK")


def resident_mb(roots):
    """What the trees of `roots` hold now, summed, in megabytes."""
    pids = {p for root in roots if root for p in tree(root)}
    return round(sum((held_kb(p) or (0, 0))[0] for p in pids) / 1024)


class Sampler:
    """The most the processes it watches held at one moment, summed, sampled every `interval`
    seconds on a thread of its own, and the most each process held.
    `add` watches a process and, from then on, everything below it."""

    def __init__(self, interval=INTERVAL):
        self.interval, self.roots, self.most, self.uncounted = interval, set(), {}, set()
        self.peak_kb, self.samples, self._stop, self._thread = 0, 0, threading.Event(), None
        # The kernel's peak of each process there at the first sample: one from before the
        # sampler started counts only once it is passed.
        self.before = {}
        self._lock = threading.Lock()

    def add(self, *pids, itself=True):
        """Watches `pids` and what they start; with `itself` false, only what they start."""
        with self._lock:
            self.roots.update(p for p in pids if p)
            if not itself:
                self.uncounted.update(p for p in pids if p)
        return self

    def sample(self):
        with self._lock:
            roots = list(self.roots)
        held, high, gone = {}, {}, []
        for root in roots:
            for pid in tree(root):
                if pid in held or pid in self.uncounted:
                    continue
                kb = held_kb(pid)
                if kb is None:
                    if pid == root:
                        gone.append(pid)
                    continue
                held[pid], high[pid] = kb
        with self._lock:
            # What it saw once it keeps watching: a daemon outlives the command that started it.
            self.roots.difference_update(gone)
            self.roots.update(held)
            for pid, kb in high.items():
                if not self.samples:
                    self.before[pid] = kb
                counted = kb if kb > self.before.get(pid, 0) else held[pid]
                self.most[pid] = max(self.most.get(pid, 0), counted)
            self.peak_kb = max(self.peak_kb, sum(held.values()))
            self.samples += 1
        return sum(held.values())

    def _loop(self):
        while not self._stop.is_set():
            began = time.monotonic()
            self.sample()
            self._stop.wait(max(0.0, self.interval - (time.monotonic() - began)))

    def start(self):
        # The thread that waits for a timed command takes the interpreter back from this one
        # within the switch interval: 5 ms by default, which an edit of 60 ms would show.
        self._switch = sys.getswitchinterval()
        sys.setswitchinterval(min(self._switch, 1e-4))
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()
        return self

    def stop(self):
        if self._thread:
            self._stop.set()
            self._thread.join()
            self._thread = None
            sys.setswitchinterval(self._switch)
            self.sample()
        return self

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.stop()

    @property
    def peak_mb(self):
        return round(self.peak_kb / 1024)

    @property
    def largest_mb(self):
        return round(max(self.most.values(), default=0) / 1024)


def run(argv, cwd=None, env=None, timeout=None, sampler=None, text=True, stdin=subprocess.DEVNULL, meter=None):
    """Runs `argv` to its end as `subprocess.run(capture_output=True)` does. The result is
    (seconds, the CompletedProcess, the memory): `largest_mb` is the peak of the largest process
    of its tree (`wait4`'s, exact, or the sampler's when more), `peak_mb` the most the tree held
    at once (sampled, and never below `largest_mb`). With `sampler`, the command is watched by
    it (beside what it already watches), `largest_mb` is `wait4`'s alone and `peak_mb` None:
    the caller has the sampler's; without, by one of its own. With `meter` (a
    counters.Meter), the command and what it starts are counted by it: on Linux it starts
    stopped until its counter is open, and the time is taken from when it was let go. `cpu_s`
    is the CPU time, user and system, of the command and of what it started and waited for (on
    Windows, everything it started, a daemon included, up to when it ended); a daemon already
    running is not in it (`tree_cpu_seconds` of its pid, before and after, is).
    On Windows the command runs in a job object (`Job`), whose peaks are exact: `peak_mb` is the
    job's, or the sampler's when more (a daemon started before, which `sampler` watches)."""
    own = sampler is None
    sampler = sampler or Sampler()
    job = Job() if WINDOWS else None
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        began = time.time()
        kwargs = dict(cwd=cwd, env=env, stdin=stdin, stdout=out, stderr=err)
        if job is not None:
            child = job.start(argv, meter.popen if meter is not None else subprocess.Popen, **kwargs)
        elif meter is not None:
            child = meter.popen(argv, **kwargs)
            began = time.time()
        else:
            child = subprocess.Popen(argv, **kwargs)
        sampler.add(child.pid)
        if own:
            sampler.start()
        fired = threading.Event()
        timer = threading.Timer(timeout, lambda: (fired.set(), child.kill())) if timeout else None
        if timer:
            timer.start()
        try:
            if job is not None:
                child.wait()
                largest, job_peak = job.peaks_mb()
                cpu = job.cpu_s()
                if job.active():
                    # What the command left running (a daemon) stays in the job, and so does
                    # all it starts later: the job is kept to read their CPU time by
                    # (`tree_cpu_seconds`).
                    _KEPT_JOBS.append(job)
                else:
                    job.close()
            else:
                if meter is not None:
                    # Exited and not reaped: macOS's totals of it are read now, the last time they can be.
                    os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT)
                    meter.exited(child.pid)
                largest, cpu = waited(child)
                job_peak = 0
        finally:
            if timer:
                timer.cancel()
        took = time.time() - began
        if own:
            sampler.stop()
        if fired.is_set():
            raise subprocess.TimeoutExpired(argv, timeout)
        said = []
        for f in (out, err):
            f.seek(0)
            data = f.read()
            said.append(data.decode("utf-8", errors="replace") if text else data)
    if own:
        largest = max(largest, sampler.largest_mb)
    # Given a sampler, the caller has its peak; the job's own, exact, is the command's alone.
    peak = max(largest, job_peak, sampler.peak_mb) if own else max(largest, job_peak) if job is not None else None
    used = dict(largest_mb=largest, peak_mb=peak, cpu_s=round(cpu, 3))
    return took, subprocess.CompletedProcess(argv, child.returncode, *said), used


def waited(child):
    """Waits for the `subprocess.Popen` `child` with `wait4`, sets its `returncode`, and
    returns the exact peak in megabytes of the largest process of its tree and the CPU time,
    user and system, of it and every process of its tree it waited for. Unix only."""
    _, status, usage = os.wait4(child.pid, 0)
    child.returncode = os.waitstatus_to_exitcode(status)
    return maxrss_mb(usage), usage.ru_utime + usage.ru_stime


def in_child(fn, *args):
    """Calls `fn(*args)` in a forked process and returns (its result, its memory): `largest_mb`,
    the exact peak of the largest process anything it ran held, and `peak_mb`, the most they
    held at once (sampled). The result has to pickle; an exception in it is raised here again.
    Unix only; fork before any thread of the caller's is started."""
    import pickle
    read, write = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(read)
        try:
            value = ("ok", fn(*args))
        except BaseException as error:  # noqa: B036 - handed to the parent as it is
            value = ("error", error)
        try:
            data = pickle.dumps(value + (maxrss_mb(resource.getrusage(resource.RUSAGE_CHILDREN)),))
        except Exception as error:
            data = pickle.dumps(("error", RuntimeError(f"in_child: {error}"), 0))
        with os.fdopen(write, "wb") as f:
            f.write(data)
        os._exit(0)
    os.close(write)
    with Sampler().add(pid, itself=False) as sampler, os.fdopen(read, "rb") as f:
        data = f.read()
        os.waitpid(pid, 0)
    how, value, largest = pickle.loads(data)
    if how == "error":
        raise value
    # The forked process is a copy of the caller, not part of what it ran.
    largest = max(largest, sampler.largest_mb)
    return value, dict(largest_mb=largest, peak_mb=max(largest, sampler.peak_mb))


# FS_IOC_FIEMAP, and what an extent's flags say: its place on disk is not known yet (delayed
# allocation), or it is shared with another file (a reflink, a clone).
FIEMAP, FIEMAP_UNKNOWN, FIEMAP_SHARED, FIEMAP_LAST = 0xC020660B, 0x2, 0x2000, 0x1


def shared_extents(path, fd):
    """The extents of the file `fd` that it shares with other files, as (start, length) on
    its device, and the bytes of the rest; None where the kernel does not say (not Linux,
    or a filesystem without FIEMAP)."""
    import fcntl, struct
    shared, own, start = [], 0, 0
    while True:
        # Up to 256 extents a call: a header of 32 bytes, 56 a extent.
        answer = bytearray(struct.pack("=QQIIII", start, (1 << 64) - 1 - start, 0, 0, 256, 0) + bytes(56 * 256))
        try:
            fcntl.ioctl(fd, FIEMAP, answer)
        except OSError:
            return None
        mapped = struct.unpack_from("=I", answer, 20)[0]
        if mapped == 0:
            return shared, own
        for at in range(mapped):
            logical, physical, length, _, _, flags = struct.unpack_from("=QQQQQI", answer, 32 + 56 * at)
            if flags & FIEMAP_SHARED and not flags & FIEMAP_UNKNOWN:
                shared.append((physical, length))
            else:
                own += length
            start = logical + length
            if flags & FIEMAP_LAST:
                return shared, own


def _allocated(path):
    """Windows: the bytes the file at `path` has allocated on disk (FILE_STANDARD_INFO's
    AllocationSize), or 0 when it is gone."""
    import msvcrt

    class Standard(ctypes.Structure):
        _fields_ = [("AllocationSize", ctypes.c_int64), ("EndOfFile", ctypes.c_int64),
                    ("NumberOfLinks", ctypes.c_uint32), ("DeletePending", ctypes.c_ubyte), ("Directory", ctypes.c_ubyte)]

    try:
        fd = os.open(path, os.O_RDONLY | getattr(os, "O_BINARY", 0))
    except FileNotFoundError:
        return 0
    try:
        info = Standard()
        get = _k32.GetFileInformationByHandleEx
        get.argtypes = [_w.HANDLE, ctypes.c_int, ctypes.c_void_p, _w.DWORD]
        if not get(msvcrt.get_osfhandle(fd), 1, ctypes.byref(info), ctypes.sizeof(info)):
            raise _win_error(f"GetFileInformationByHandleEx of {path}")
        return info.AllocationSize
    finally:
        os.close(fd)


def disk_mb(*paths):
    """What the files under `paths` take on disk in megabytes: blocks allocated, each file
    once however many links it has (cargo links what it builds into target/debug), and on
    Linux each block once however many files share it (a reflink: a copy on XFS or Btrfs).
    Where the kernel does not say what is shared (macOS's clones, which APFS does not tell
    apart, and ReFS's block clones on Windows) a file counts as `du` counts it, all of its
    blocks."""
    seen, total, shared = set(), 0, {}
    for path in paths:
        for root, dirs, files in os.walk(path):
            for name in files + dirs:
                at = os.path.join(root, name)
                try:
                    st = os.lstat(at)
                except OSError:
                    continue
                if (st.st_dev, st.st_ino) in seen:
                    continue
                seen.add((st.st_dev, st.st_ino))
                if WINDOWS:
                    # No st_blocks: the file's allocation, as Explorer's "size on disk" says it.
                    # A directory's entries live in the file system's own table, not counted.
                    if stat.S_ISREG(st.st_mode):
                        total += _allocated(at)
                    continue
                extents = None
                if sys.platform == "linux" and stat.S_ISREG(st.st_mode) and st.st_blocks:
                    try:
                        fd = os.open(at, os.O_RDONLY | os.O_NOFOLLOW)
                    except OSError:
                        fd = None
                    if fd is not None:
                        try:
                            extents = shared_extents(at, fd)
                        finally:
                            os.close(fd)
                if extents is None or not extents[0]:
                    total += st.st_blocks * 512
                    continue
                # What the file has beyond its extents is preallocated past its end.
                runs, own = extents
                total += max(st.st_blocks * 512 - own - sum(length for _, length in runs), 0) + own
                shared.setdefault(st.st_dev, []).extend(runs)
    for runs in shared.values():
        end = -1
        for start, length in sorted(runs):
            if start + length > end:
                total += start + length - max(start, end)
                end = start + length
    return round(total / 1e6)


def selftest():
    """What this measures, against processes of known size: a command holding 300 MB that
    starts one holding 200 MB beside it, and burns half a second of CPU. Each holds its memory,
    every page written so that it is resident, until both are there at once."""
    child = ("import os, sys, time\n"
             "n = int(sys.argv[1]) << 20\n"
             "b = bytearray(n)\n"
             "b[::4096] = b'\\1' * len(range(0, n, 4096))\n"
             "open(sys.argv[2], 'w').close()\n"
             "deadline = time.time() + 30\n"
             "while not os.path.exists(sys.argv[3]) and time.time() < deadline:\n"
             "    time.sleep(0.01)\n"
             "time.sleep(0.2)\n")
    parent = ("import subprocess, sys, time\n"
              "code, mine, theirs, go = sys.argv[1:]\n"
              "kid = subprocess.Popen([sys.executable, '-c', code, '200', theirs, mine + '.go'])\n"
              "end = time.process_time() + 0.5\n"
              "while time.process_time() < end:\n"
              "    pass\n"
              "exec(code.replace('sys.argv[1]', '\"300\"').replace('sys.argv[2]', 'mine').replace('sys.argv[3]', 'go'))\n"
              "kid.wait()\n")
    with tempfile.TemporaryDirectory() as tmp:
        mine, theirs = os.path.join(tmp, "a"), os.path.join(tmp, "b")
        go = mine + ".go"
        # Both ready, then let go: the parent waits on `go`, the child on `mine.go`, the same file.
        def release():
            deadline = time.time() + 60
            while not (os.path.exists(mine) and os.path.exists(theirs)) and time.time() < deadline:
                time.sleep(0.01)
            seen.extend(tree(started[0]) if started else [])
            open(go, "w").close()
        seen, started = [], []
        real_popen = subprocess.Popen

        def popen(*args, **kwargs):
            started.append((p := real_popen(*args, **kwargs)).pid)
            return p
        subprocess.Popen = popen
        try:
            waiter = threading.Thread(target=release)
            waiter.start()
            took, ran, used = run([sys.executable, "-c", parent, child, mine, theirs, go], timeout=120)
            waiter.join()
        finally:
            subprocess.Popen = real_popen
        assert ran.returncode == 0, ran.stderr
        assert 290 <= used["largest_mb"] <= 400, used
        assert 480 <= used["peak_mb"] <= 700, used
        assert 0.4 <= used["cpu_s"] <= took + 1, (used, took)
        assert len(seen) >= 2, f"the tree of the command had {seen}: its child is missing"
        with open(os.path.join(tmp, "file"), "wb") as f:
            f.write(b"x" * 3_000_000)
        assert disk_mb(tmp) >= 3, disk_mb(tmp)
    assert (held_kb(os.getpid()) or (0, 0))[0] > 1024
    assert (cpu_seconds(os.getpid()) or 0) > 0
    if not DARWIN:
        machine = system_mb()
        assert machine and machine["total"] > 0 and 0 < machine["used"] <= machine["total"], machine
    print(f"memory.py selftest: ok (largest {used['largest_mb']} MB, at once {used['peak_mb']} MB, cpu {used['cpu_s']} s)")


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        selftest()
        sys.exit(0)
    # memory.py COMMAND...: what a command's tree held, as the benchmarks measure it.
    took, ran, used = run(sys.argv[1:], text=False, stdin=None)
    sys.stdout.buffer.write(ran.stdout)
    sys.stderr.buffer.write(ran.stderr)
    print(f"{took:.3f} s, largest process {used['largest_mb']} MB, the tree at once {used['peak_mb']} MB", file=sys.stderr)
    sys.exit(ran.returncode)
