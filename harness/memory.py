"""memory.py: how much memory a process tree holds, the same way on Linux and macOS.

The one helper the benchmarks measure memory with (bench.py, runbench.py, apps.py); a
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
sampler started, such as a daemon, only a peak above the one it had then). Python 3 standard
library only.
"""
import ctypes, os, platform, resource, stat, subprocess, sys, tempfile, threading, time

DARWIN = platform.system() == "Darwin"
INTERVAL = float(os.environ.get("BLAZE_MEMORY_INTERVAL", "0.01"))
PAGE_KB = os.sysconf("SC_PAGE_SIZE") // 1024 if hasattr(os, "sysconf") else 4

if DARWIN:
    _proc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    _RUSAGE_INFO_V4 = 4
    # struct rusage_info_v4 (<sys/resource.h>): a UUID, then 35 counters of 64 bits.
    _FOOTPRINT, _LIFETIME_MAX_FOOTPRINT = 7, 28

    class _RusageInfo(ctypes.Structure):
        _fields_ = [("uuid", ctypes.c_uint8 * 16), ("counters", ctypes.c_uint64 * 35)]


def maxrss_mb(usage):
    """`ru_maxrss` of a `resource.struct_rusage` in megabytes: Linux counts it in KiB, macOS in bytes."""
    return round(usage.ru_maxrss / (2**20 if DARWIN else 2**10))


def held_kb(pid):
    """What `pid` holds now and the most it has held, in KiB, or None when it is gone (or a
    zombie, which holds nothing). On Linux the most is what it holds now: the sampler keeps
    the peak."""
    if DARWIN:
        info = _RusageInfo()
        if _proc.proc_pid_rusage(pid, _RUSAGE_INFO_V4, ctypes.byref(info)) != 0 or not info.counters[_FOOTPRINT]:
            return None
        return info.counters[_FOOTPRINT] // 1024, info.counters[_LIFETIME_MAX_FOOTPRINT] // 1024
    try:
        with open(f"/proc/{pid}/statm") as f:
            pages = int(f.read().split()[1])
    except (OSError, ValueError, IndexError):
        return None
    return (pages * PAGE_KB, pages * PAGE_KB) if pages else None


def children(pid):
    """The processes `pid` started that still run."""
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
    found, todo = [], [root]
    while todo:
        pid = todo.pop()
        found.append(pid)
        todo += children(pid)
    return found


def system_mb():
    """What the whole machine has and uses now, in megabytes: `total`, `used` (all but what
    the kernel could hand out without swapping, `MemAvailable`), `swap_total` and `swap_used`.
    None off Linux."""
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
    """The CPU time `pid` has used, user and system, in seconds; None when it is gone or off
    Linux."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()
    except (OSError, IndexError):
        return None
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


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


def run(argv, cwd=None, env=None, timeout=None, sampler=None, text=True, stdin=subprocess.DEVNULL):
    """Runs `argv` to its end as `subprocess.run(capture_output=True)` does. The result is
    (seconds, the CompletedProcess, the memory): `largest_mb` is the peak of the largest process
    of its tree (`wait4`'s, exact, or the sampler's when more), `peak_mb` the most the tree held
    at once (sampled, and never below `largest_mb`). With `sampler`, the command is watched by
    it (beside what it already watches), `largest_mb` is `wait4`'s alone and `peak_mb` None:
    the caller has the sampler's; without, by one of its own."""
    own = sampler is None
    sampler = sampler or Sampler()
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        began = time.time()
        child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=stdin, stdout=out, stderr=err)
        sampler.add(child.pid)
        if own:
            sampler.start()
        fired = threading.Event()
        timer = threading.Timer(timeout, lambda: (fired.set(), child.kill())) if timeout else None
        if timer:
            timer.start()
        try:
            largest = waited(child)
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
    used = dict(largest_mb=largest, peak_mb=max(largest, sampler.peak_mb) if own else None)
    return took, subprocess.CompletedProcess(argv, child.returncode, *said), used


def waited(child):
    """Waits for the `subprocess.Popen` `child` with `wait4`, sets its `returncode`, and
    returns the exact peak in megabytes of the largest process of its tree."""
    _, status, usage = os.wait4(child.pid, 0)
    child.returncode = os.waitstatus_to_exitcode(status)
    return maxrss_mb(usage)


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


def disk_mb(*paths):
    """What the files under `paths` take on disk in megabytes: blocks allocated, each file
    once however many links it has (cargo links what it builds into target/debug), and on
    Linux each block once however many files share it (a reflink: a copy on XFS or Btrfs).
    Where the kernel does not say what is shared (macOS's clones, which APFS does not tell
    apart) a file counts as `du` counts it, all of its blocks."""
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


if __name__ == "__main__":
    # memory.py COMMAND...: what a command's tree held, as the benchmarks measure it.
    took, ran, used = run(sys.argv[1:], text=False, stdin=None)
    sys.stdout.buffer.write(ran.stdout)
    sys.stderr.buffer.write(ran.stderr)
    print(f"{took:.3f} s, largest process {used['largest_mb']} MB, the tree at once {used['peak_mb']} MB", file=sys.stderr)
    sys.exit(ran.returncode)
