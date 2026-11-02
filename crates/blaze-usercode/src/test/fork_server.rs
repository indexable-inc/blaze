// blaze's fork server, the harness side of the protocol in `wire.rs`, which comes before this
// in the same module. Text only: `blaze_usercode::test::fork_server` puts both in the test
// harness blaze writes, as the module `fork_server` beside the harness's `main`, where
// `super::test` is libtest, followed by the module `exec` of `fork_server/recorded.rs` or
// `fork_server/unrecorded.rs` (whether the binary has jump-table slots). Never a module of
// blaze-usercode. It is compiled in the tested crate's edition, so it names everything from
// `::std` and `::core`.
//
// What happens once per binary happens before the first test: the names of the hello, the
// child-exit queue, the slot count, the measure of one fork. A test costs one socket pair, one
// fork and one frame; the buffers of the loop and the execution records (with slots) are kept
// across tests. One server forks one test at a time, so blaze may ask for replicas
// (`Frame::Replicate`): copies forked before any test runs, each a server on a connection of
// its own.

/// Serves `tests` to blaze over the socket at `socket` (the value of [`SOCKET`]), then exits:
/// what the harness's `main` calls when the variable is set.
#[cfg(unix)]
pub fn serve(tests: &'static [&'static super::test::TestDescAndFn], socket: ::std::ffi::OsString) -> ! {
    let code = match unix::serve(tests, &socket) {
        Ok(()) => 0,
        Err(why) => {
            eprintln!("blaze's fork server: {}", why);
            101
        }
    };
    ::std::process::exit(code)
}

#[cfg(not(unix))]
pub fn serve(tests: &'static [&'static super::test::TestDescAndFn], socket: ::std::ffi::OsString) -> ! {
    let _ = (tests, socket);
    eprintln!("blaze's fork server: this platform has no fork; the package must run its tests as libtest's threads");
    ::std::process::exit(101)
}

#[cfg(unix)]
mod unix {
    use super::super::test;
    use super::exec::{Exec, Region};
    use super::{
        End, EndView, ExecView, Frames, Hits, LONGEST_OUTPUT, MARK, SOCKET, VERSION, View, encode_ended, encode_hello,
    };
    use ::std::io::{Read, Write};
    use ::std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use ::std::os::unix::net::UnixStream;

    type Int = ::core::ffi::c_int;
    #[cfg(target_os = "linux")]
    type Nfds = ::core::ffi::c_ulong;
    #[cfg(not(target_os = "linux"))]
    type Nfds = ::core::ffi::c_uint;

    #[repr(C)]
    struct PollFd {
        fd: Int,
        events: i16,
        revents: i16,
    }

    const POLLIN: i16 = 0x1;
    const WNOHANG: Int = 1;
    const SIGKILL: Int = 9;

    unsafe extern "C" {
        fn fork() -> Int;
        fn waitpid(pid: Int, status: *mut Int, options: Int) -> Int;
        fn kill(pid: Int, signal: Int) -> Int;
        fn setpgid(pid: Int, group: Int) -> Int;
        fn dup2(from: Int, to: Int) -> Int;
        fn close(fd: Int) -> Int;
        fn poll(fds: *mut PollFd, count: Nfds, timeout: Int) -> Int;
        fn access(path: *const ::core::ffi::c_char, mode: Int) -> Int;
        fn _exit(code: Int) -> !;
    }

    /// A test's child: its process (the leader of its own group), where its output comes in,
    /// and how it ended once it has.
    struct Child {
        id: u64,
        pid: Int,
        output: UnixStream,
        bytes: Vec<u8>,
        cut: bool,
        open: bool,
        status: Option<Int>,
        /// Readable once the child has ended, where each child has one (Linux's pidfd).
        exit: Option<OwnedFd>,
        /// Where it notes what it executed, read once it has ended.
        region: Region,
    }

    // What wakes the server the moment a child ends, so it never waits on a clock: a pidfd
    // per child on Linux, one kqueue for all of them on Apple's systems.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod exits {
        use super::{FromRawFd, Int, OwnedFd};

        unsafe extern "C" {
            fn syscall(number: ::core::ffi::c_long, ...) -> ::core::ffi::c_long;
        }

        /// `pidfd_open`, the same number on every Linux architecture.
        const PIDFD_OPEN: ::core::ffi::c_long = 434;

        pub fn queue() -> Result<Option<OwnedFd>, String> {
            Ok(None)
        }

        /// The child's pidfd, which is close-on-exec. `pid` is not reaped yet, so it is there
        /// even if it has ended.
        pub fn watch(_queue: &Option<OwnedFd>, pid: Int) -> Result<(Option<OwnedFd>, bool), String> {
            // SAFETY: `pidfd_open` takes a pid and flags by value and touches no memory of
            // ours; the variadic arguments are passed as `long`, the width the kernel reads.
            // A wrong number would run another syscall, which is why `PIDFD_OPEN` is pinned.
            let fd = unsafe { syscall(PIDFD_OPEN, pid as ::core::ffi::c_long, 0 as ::core::ffi::c_long) };
            if fd < 0 {
                return Err(format!("pidfd_open: {}", ::std::io::Error::last_os_error())); // alloc: error path
            }
            // SAFETY: `fd` is >= 0, so `pidfd_open` just made it, and no other owner of it
            // exists; were there one, the two would close it twice.
            Ok((Some(unsafe { OwnedFd::from_raw_fd(fd as Int) }), false))
        }

        pub fn clear(_queue: &Option<OwnedFd>) {}

        /// In a replica just forked: nothing, as there is no queue.
        pub fn forked(_queue: &mut Option<OwnedFd>) -> Result<(), String> {
            Ok(())
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos", target_os = "watchos", target_os = "visionos"))]
    mod exits {
        use super::{AsRawFd, FromRawFd, Int, OwnedFd};

        #[repr(C)]
        struct Kevent {
            ident: usize,
            filter: i16,
            flags: u16,
            fflags: u32,
            data: isize,
            udata: *mut ::core::ffi::c_void,
        }

        #[repr(C)]
        struct Timespec {
            seconds: ::core::ffi::c_long,
            nanoseconds: ::core::ffi::c_long,
        }

        const EVFILT_PROC: i16 = -5;
        const EV_ADD: u16 = 0x1;
        const EV_ONESHOT: u16 = 0x10;
        const NOTE_EXIT: u32 = 0x8000_0000;
        const ESRCH: i32 = 3;

        unsafe extern "C" {
            fn kqueue() -> Int;
            fn kevent(
                queue: Int,
                changes: *const Kevent,
                changed: Int,
                events: *mut Kevent,
                most: Int,
                timeout: *const Timespec,
            ) -> Int;
        }

        /// A kqueue, which a forked child doesn't inherit.
        pub fn queue() -> Result<Option<OwnedFd>, String> {
            // SAFETY: `kqueue` takes no arguments and touches no memory of ours.
            let fd = unsafe { kqueue() };
            if fd < 0 {
                return Err(format!("kqueue: {}", ::std::io::Error::last_os_error())); // alloc: error path
            }
            // SAFETY: `fd` is >= 0, so `kqueue` just made it, and no other owner of it
            // exists; were there one, the two would close it twice.
            Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        /// Asks the kqueue for `pid`'s end. A child that has already ended can't be watched
        /// (`ESRCH`): the server looks at once instead.
        pub fn watch(queue: &Option<OwnedFd>, pid: Int) -> Result<(Option<OwnedFd>, bool), String> {
            let queue = queue.as_ref().expect("a kqueue on this system");
            let change = Kevent {
                ident: pid as usize,
                filter: EVFILT_PROC,
                flags: EV_ADD | EV_ONESHOT,
                fflags: NOTE_EXIT,
                data: 0,
                udata: ::core::ptr::null_mut(),
            };
            // SAFETY: `change` is a live `Kevent` laid out as the kernel's (`repr(C)`, the
            // fields of `<sys/event.h>`), read for the count 1 passed; the event list is null
            // with room 0, so nothing is written, and the null timeout is allowed.
            let done = unsafe { kevent(queue.as_raw_fd(), &change, 1, ::core::ptr::null_mut(), 0, ::core::ptr::null()) };
            if done < 0 {
                let error = ::std::io::Error::last_os_error();
                if error.raw_os_error() == Some(ESRCH) {
                    return Ok((None, true));
                }
                return Err(format!("kevent: {}", error)); // alloc: error path
            }
            Ok((None, false))
        }

        /// Takes the ends the kqueue has, so it is not readable for them again; `waitpid`
        /// says which children they were.
        pub fn clear(queue: &Option<OwnedFd>) {
            let queue = queue.as_ref().expect("a kqueue on this system");
            let zero = Timespec { seconds: 0, nanoseconds: 0 };
            // SAFETY: every field of `Kevent` is an integer or a raw pointer, for which all
            // zero bytes are a valid value (0 and null).
            let mut events: [Kevent; 16] = unsafe { ::core::mem::zeroed() };
            loop {
                // SAFETY: `events` is live and holds the 16 entries the call may write, no more
                // than that count is passed, and `zero` is a live timeout, so the call returns
                // at once. A larger count would let the kernel write past the array.
                let got = unsafe { kevent(queue.as_raw_fd(), ::core::ptr::null(), 0, events.as_mut_ptr(), 16, &zero) };
                if got < 16 {
                    return;
                }
            }
        }

        /// In a replica just forked: a kqueue of its own. The one it was forked with is not
        /// its (a fork does not inherit a kqueue), so its descriptor is let go unclosed: that
        /// number may be open again for something else of the replica's by the time it would
        /// close.
        pub fn forked(queue: &mut Option<OwnedFd>) -> Result<(), String> {
            if let Some(old) = queue.take() {
                ::core::mem::forget(old);
            }
            *queue = self::queue()?;
            Ok(())
        }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    )))]
    mod exits {
        use super::{Int, OwnedFd};

        pub fn queue() -> Result<Option<OwnedFd>, String> {
            Err("blaze's fork server waits on a pidfd (Linux) or a kqueue (Apple), and this system has neither it knows; the package must run its tests as libtest's threads".to_owned()) // alloc: error path
        }

        pub fn watch(_queue: &Option<OwnedFd>, _pid: Int) -> Result<(Option<OwnedFd>, bool), String> {
            Err("no way to wait on a child here".to_owned()) // alloc: error path
        }

        pub fn clear(_queue: &Option<OwnedFd>) {}

        pub fn forked(_queue: &mut Option<OwnedFd>) -> Result<(), String> {
            Err("no way to wait on a child here".to_owned()) // alloc: error path
        }
    }

    // What a test's child closes: every descriptor of the server's (its socket to blaze, its
    // children's outputs and pidfds, the child's own ends), which is every one at 3 or above
    // that was not open when the server started, since no code of the program runs in the
    // server after. On Linux that is one `close_range` per gap in what the program had open,
    // usually one, where a `close` per descriptor costs two per test in flight.
    #[cfg(target_os = "linux")]
    mod fds {
        use super::super::closed_ranges;
        use super::{Int, dup2};

        unsafe extern "C" {
            fn syscall(number: ::core::ffi::c_long, ...) -> ::core::ffi::c_long;
            fn fcntl(fd: Int, command: Int, ...) -> Int;
        }

        /// `close_range`, the same number on every Linux architecture.
        const CLOSE_RANGE: ::core::ffi::c_long = 436;
        const F_GETFD: Int = 1;

        /// The ranges a test's child closes.
        pub struct Kept(Vec<(u32, u32)>);

        impl Kept {
            /// What is open now, which every test's child keeps; an error on a kernel without
            /// `close_range` (before 5.9).
            pub fn now() -> Result<Kept, String> {
                // SAFETY: `close_range` takes integers and touches no memory of ours; the range
                // of the one descriptor `u32::MAX`, never open, closes nothing and only asks
                // whether the kernel has the call.
                let asked = unsafe {
                    syscall(CLOSE_RANGE, u32::MAX as ::core::ffi::c_long, u32::MAX as ::core::ffi::c_long, 0 as ::core::ffi::c_long)
                };
                if asked != 0 {
                    return Err(format!("close_range: {} (blaze's fork server needs Linux 5.9 or later)", ::std::io::Error::last_os_error())); // alloc: error path
                }
                let mut open: Vec<u32> = Vec::new(); // alloc: setup, once per server
                let listing = ::std::fs::read_dir("/proc/self/fd").map_err(|error| format!("listing /proc/self/fd: {}", error))?; // alloc: error path
                for entry in listing {
                    let entry = entry.map_err(|error| format!("listing /proc/self/fd: {}", error))?; // alloc: error path
                    if let Some(fd) = entry.file_name().to_str().and_then(|it| it.parse::<u32>().ok()) {
                        open.push(fd);
                    }
                }
                // The listing's own descriptor was in it, and is closed by now.
                // SAFETY: F_GETFD takes integers and touches no memory of ours; it only asks
                // whether `fd` is open.
                open.retain(|&fd| unsafe { fcntl(fd as Int, F_GETFD) } >= 0);
                let mut gaps = Vec::new(); // alloc: setup, once per server
                closed_ranges(&mut open, &mut gaps);
                Ok(Kept(gaps))
            }

            /// In the test's child: its output on 1 and 2, and every descriptor of the server's
            /// closed. `_servers` is what the other platforms close one by one.
            pub fn into_child(&self, theirs: Int, _servers: impl Iterator<Item = Int>) {
                // SAFETY: `theirs` is open until the closes below; 1 and 2 become copies of it,
                // and stay, being below 3.
                unsafe { dup2(theirs, 1) };
                // SAFETY: as above.
                unsafe { dup2(theirs, 2) };
                for &(first, last) in &self.0 {
                    // SAFETY: in the test's child, which leaves by `_exit` and never uses the
                    // server's owners again (`start`), so no descriptor is closed twice or used
                    // after its close; `theirs` is in a gap, closed here after its copies.
                    unsafe { syscall(CLOSE_RANGE, first as ::core::ffi::c_long, last as ::core::ffi::c_long, 0 as ::core::ffi::c_long) };
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    mod fds {
        use super::{Int, close, dup2};

        /// Nothing: the other platforms close the server's descriptors one by one.
        pub struct Kept;

        impl Kept {
            pub fn now() -> Result<Kept, String> {
                Ok(Kept)
            }

            /// In the test's child: every descriptor of `servers` closed, and its output on 1
            /// and 2.
            pub fn into_child(&self, theirs: Int, servers: impl Iterator<Item = Int>) {
                for fd in servers {
                    // SAFETY: in the test's child, which leaves by `_exit` and never uses the
                    // server's owners again (`start`), so no descriptor is closed twice or used
                    // after its close.
                    unsafe { close(fd) };
                }
                // SAFETY: `theirs` is open until the close below; 1 and 2 become copies of it.
                unsafe { dup2(theirs, 1) };
                // SAFETY: as above.
                unsafe { dup2(theirs, 2) };
                if theirs > 2 {
                    // SAFETY: as above, `theirs` is not used in the child after this; 1 and 2 stay.
                    unsafe { close(theirs) };
                }
            }
        }
    }

    pub fn serve(tests: &'static [&'static test::TestDescAndFn], socket: &::std::ffi::OsStr) -> Result<(), String> {
        // Taken out before any test runs, so a test that runs its own binary gets libtest.
        // SAFETY: the harness's `main` calls this before it starts any thread, and the server
        // never starts one (a test's thread runs in a forked child), so no other thread reads
        // the environment as it changes. A thread here would make this a data race.
        unsafe {
            ::std::env::remove_var(SOCKET);
        }
        let kept = fds::Kept::now()?;
        let fork = fork_cost()?;
        let mut queue = exits::queue()?;
        let mut exec = Exec::open();
        // A child ended before its end could be watched: the next poll doesn't wait.
        let mut now = false;
        // Kept across turns of the loop, so a turn allocates nothing: what is polled, which
        // children's outputs those are, the frames to send (one write a turn), the output
        // buffers of children that ended, for the next ones, and the hits of the last.
        let mut watched: Vec<usize> = Vec::new(); // alloc: empty, allocates nothing
        let mut fds: Vec<PollFd> = Vec::new(); // alloc: empty, allocates nothing
        let mut out: Vec<u8> = Vec::new(); // alloc: empty, allocates nothing
        let mut spare: Vec<Vec<u8>> = Vec::new(); // alloc: empty, allocates nothing
        let mut hits: Vec<u8> = Vec::new(); // alloc: empty, allocates nothing
        let mut stream = connect(socket)?;
        hello(&mut stream, &mut out, tests, fork)?;
        let mut frames = Frames::new();
        let mut children: Vec<Child> = Vec::new(); // alloc: empty, allocates nothing
        let mut quitting = false;
        // Whether blaze's first frame has come: replicas are made only before it, so a copy
        // has no test running, no region of the server's and no frame meant for the server.
        let mut first = true;
        let mut buffer = vec![0u8; 1 << 16]; // alloc: setup, once per server
        loop {
            if quitting && children.is_empty() {
                return Ok(());
            }
            watched.clear();
            watched.extend((0..children.len()).filter(|&at| children[at].open));
            fds.clear();
            fds.push(PollFd { fd: stream.as_raw_fd(), events: POLLIN, revents: 0 });
            for &at in &watched {
                fds.push(PollFd { fd: children[at].output.as_raw_fd(), events: POLLIN, revents: 0 });
            }
            if let Some(queue) = &queue {
                fds.push(PollFd { fd: queue.as_raw_fd(), events: POLLIN, revents: 0 });
            }
            for child in children.iter().filter(|it| it.status.is_none()) {
                if let Some(exit) = &child.exit {
                    fds.push(PollFd { fd: exit.as_raw_fd(), events: POLLIN, revents: 0 });
                }
            }
            let timeout = if now { 0 } else { -1 };
            now = false;
            // SAFETY: `fds` holds `fds.len()` entries for the length of the call.
            let ready = unsafe { poll(fds.as_mut_ptr(), fds.len() as Nfds, timeout) };
            if ready < 0 {
                let error = ::std::io::Error::last_os_error();
                if error.kind() == ::std::io::ErrorKind::Interrupted {
                    continue;
                }
                kill_all(&mut children);
                return Err(format!("poll: {}", error)); // alloc: error path
            }
            if fds[0].revents != 0 {
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        // blaze is gone: nothing will read what the tests say.
                        kill_all(&mut children);
                        return Ok(());
                    }
                    Ok(n) => frames.push(&buffer[..n]),
                    Err(ref error) if error.kind() == ::std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        kill_all(&mut children);
                        return Err(format!("reading the socket: {}", error)); // alloc: error path
                    }
                }
                loop {
                    let frame = match frames.pop_view() {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(why) => {
                            kill_all(&mut children);
                            return Err(why);
                        }
                    };
                    let was_first = ::core::mem::replace(&mut first, false);
                    match frame {
                        View::Replicate { count } => {
                            if !was_first {
                                kill_all(&mut children);
                                return Err(format!("blaze asked for {} replicas after its first frame", count)); // alloc: error path
                            }
                            if replicate(count)? {
                                // A copy: what came after the request is the server's, not its.
                                exits::forked(&mut queue)?;
                                stream = connect(socket)?;
                                frames = Frames::new();
                                hello(&mut stream, &mut out, tests, fork)?;
                                break;
                            }
                        }
                        View::Run { id, index } => match start(tests, &mut exec, &kept, id, index, &stream, &children, spare.pop().unwrap_or_default()) {
                            Ok(mut child) => {
                                match exits::watch(&queue, child.pid) {
                                    Ok((exit, ended)) => {
                                        child.exit = exit;
                                        now |= ended;
                                    }
                                    Err(why) => {
                                        children.push(child);
                                        kill_all(&mut children);
                                        return Err(why);
                                    }
                                }
                                children.push(child);
                            }
                            Err(why) => encode_ended(&mut out, id, EndView::NotStarted(&why), &[], ExecView::Not(exec.not_run(&why))),
                        },
                        View::Kill { id } => {
                            if let Some(child) = children.iter().find(|it| it.id == id && it.status.is_none()) {
                                kill_group(child.pid);
                            }
                        }
                        View::Quit => quitting = true,
                        other => {
                            kill_all(&mut children);
                            return Err(format!("blaze sent {:?}, which only a server sends", other)); // alloc: error path
                        }
                    }
                }
            }
            for (slot, &at) in watched.iter().enumerate() {
                if fds[slot + 1].revents != 0 {
                    drain(&mut children[at]);
                }
            }
            if queue.is_some() && fds[watched.len() + 1].revents != 0 {
                exits::clear(&queue);
            }
            reap(&mut children);
            let mut at = 0;
            while at < children.len() {
                let status = match children[at].status {
                    Some(status) => status,
                    None => {
                        at += 1;
                        continue;
                    }
                };
                let mut child = children.swap_remove(at);
                // What it wrote before it ended is in the socket's buffer now; a process it
                // left behind that writes later is not waited for.
                drain(&mut child);
                if child.cut {
                    // Written into the output's own buffer: a `Vec` takes it whole.
                    let _ = write!(child.bytes, "\n[blaze: the output was cut at {} bytes]\n", LONGEST_OUTPUT);
                }
                let end = end_of(status);
                let read = child.region.read(&end, &mut hits);
                let executed = match &read {
                    Ok(slots) => ExecView::Hits { slots: *slots, hits: Hits::of_bytes(&hits).expect("four bytes a hit") },
                    Err(why) => ExecView::Not(&**why),
                };
                encode_ended(&mut out, child.id, end.view(), &child.bytes, executed);
                exec.give(child.region, &hits, read.is_ok());
                // A buffer that held a long output is let go rather than kept at that size.
                if child.bytes.capacity() <= KEPT {
                    child.bytes.clear();
                    spare.push(child.bytes);
                }
            }
            // Every end of this turn in one write: blaze reads them as one batch.
            if !out.is_empty() {
                if let Err(error) = stream.write_all(&out) {
                    kill_all(&mut children);
                    return Err(format!("writing to the socket: {}", error)); // alloc: error path
                }
                out.clear();
                if out.capacity() > KEPT {
                    out.shrink_to(KEPT);
                }
            }
        }
    }

    /// The most bytes a buffer of output or of frames keeps between uses.
    const KEPT: usize = 1 << 20;

    fn connect(socket: &::std::ffi::OsStr) -> Result<UnixStream, String> {
        // blaze listens as it starts the binary; the listen may be made a moment after.
        let mut tries = 0;
        loop {
            match UnixStream::connect(socket) {
                Ok(stream) => return Ok(stream),
                Err(ref error)
                    if tries < 400
                        && (error.kind() == ::std::io::ErrorKind::NotFound
                            || error.kind() == ::std::io::ErrorKind::ConnectionRefused) =>
                {
                    tries += 1;
                    ::std::thread::sleep(::std::time::Duration::from_millis(25));
                }
                Err(error) => return Err(format!("connecting to {:?}: {}", socket, error)), // alloc: error path
            }
        }
    }

    /// The hello, encoded in `out` (left empty) from the tests' own names.
    fn hello(
        stream: &mut UnixStream,
        out: &mut Vec<u8>,
        tests: &'static [&'static test::TestDescAndFn],
        fork: u64,
    ) -> Result<(), String> {
        out.clear();
        encode_hello(out, VERSION, ::std::process::id(), fork, tests.iter().map(|it| it.desc.name.as_slice()));
        let sent = stream.write_all(&out[..]).map_err(|error| format!("writing to the socket: {}", error)); // alloc: error path
        out.clear();
        sent
    }

    /// How long one fork of the server takes, in nanoseconds, the least of three: what blaze
    /// sizes its replicas by. Each child leaves at once and is reaped here, so it reads nothing
    /// and leaves no line in the sandbox's record.
    fn fork_cost() -> Result<u64, String> {
        let mut least = u64::MAX;
        for _ in 0..3 {
            let start = ::std::time::Instant::now();
            // SAFETY: the server has one thread (see `serve`), so the child is a whole copy of
            // it; the child does nothing but `_exit`, which is async-signal-safe and runs no
            // destructor and no `atexit` handler, so nothing of the server's is used or freed
            // twice.
            let pid = unsafe { fork() };
            if pid < 0 {
                return Err(format!("fork: {}", ::std::io::Error::last_os_error())); // alloc: error path
            }
            if pid == 0 {
                // SAFETY: as above.
                unsafe { _exit(0) }
            }
            let took = start.elapsed();
            let mut status: Int = 0;
            // SAFETY: `status` is a live `int` the call writes once; `pid` is the child just
            // forked, not yet reaped, so the wait is for it alone and ends once it has left.
            unsafe { waitpid(pid, &mut status, 0) };
            least = least.min(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX));
        }
        Ok(least)
    }

    /// Forks `count` copies of the server before it runs any test, `true` in each copy. Each
    /// process gives about half of what is left to the copy it forks, so the copies come in
    /// about `log2(count + 1)` rounds of forks made side by side, not `count` one after another.
    fn replicate(count: u32) -> Result<bool, String> {
        let mut left = count;
        let mut copy = false;
        while left > 0 {
            let given = (left - 1) / 2;
            // SAFETY: the server has one thread (see `serve`), so the copy is a whole one and no
            // lock is left held by a thread that isn't there: the copy, a server again, need not
            // keep to async-signal-safe calls. No test runs yet (`Replicate` is
            // only blaze's first frame), so the copy shares no child and no execution region with
            // the server; its socket to blaze is the server's until it connects its own.
            let pid = unsafe { fork() };
            if pid < 0 {
                return Err(format!("forking a replica: {}", ::std::io::Error::last_os_error())); // alloc: error path
            }
            if pid == 0 {
                left = given;
                copy = true;
            } else {
                left -= given + 1;
            }
        }
        Ok(copy)
    }

    fn start(
        tests: &'static [&'static test::TestDescAndFn],
        exec: &mut Exec,
        kept: &fds::Kept,
        id: u64,
        index: u32,
        stream: &UnixStream,
        children: &[Child],
        bytes: Vec<u8>,
    ) -> Result<Child, String> {
        let test: &'static test::TestDescAndFn = match tests.get(index as usize) {
            Some(test) => *test,
            None => return Err(format!("the binary has {} tests, and no test {}", tests.len(), index)), // alloc: error path
        };
        let (ours, theirs) =
            UnixStream::pair().map_err(|error| format!("a socket pair for the test's output: {}", error))?; // alloc: error path
        // Mapped before the fork, so the child shares it; given back once the server has read
        // it, unmapped by its drop when the fork fails.
        let region = exec.map()?;
        // SAFETY: the server has one thread (see `serve`), so the child is a whole copy of it
        // and no lock is left held by a thread that isn't there, so the child need not keep to
        // async-signal-safe calls: it closes and copies descriptors, arms its record, then runs
        // the test (which allocates and may start threads) and leaves by `_exit`.
        let pid = unsafe { fork() };
        if pid < 0 {
            return Err(format!("fork: {}", ::std::io::Error::last_os_error())); // alloc: error path
        }
        if pid == 0 {
            // The child closes the server's descriptors under the owners that hold them: it
            // never uses those owners again, since it leaves by `_exit` below, which runs no
            // destructor, so no descriptor is closed twice or used after its close.
            // SAFETY: `setpgid` takes integers and touches no memory of ours; (0, 0) makes
            // the child lead a group of its own, which `kill_group` relies on.
            unsafe { setpgid(0, 0) };
            let servers = [stream.as_raw_fd(), ours.as_raw_fd()].into_iter().chain(
                children
                    .iter()
                    .flat_map(|child| ::core::iter::once(child.output.as_raw_fd()).chain(child.exit.as_ref().map(|it| it.as_raw_fd()))),
            );
            kept.into_child(theirs.as_raw_fd(), servers);
            let armed = region.arm();
            if armed != 0 {
                eprintln!("blaze's fork server: the runtime did not arm the execution record ({}); what this test executed is not known", armed);
            }
            let code = run(test);
            let _ = ::std::io::stdout().flush();
            let _ = ::std::io::stderr().flush();
            // SAFETY: `_exit` ends the process at once and returns to nothing; the child's
            // copies of the server's owners are never dropped, which is what the closes above
            // rely on, and the server's `atexit` handlers don't run a second time here.
            unsafe { _exit(code) }
        }
        // Made by both, so a kill sent at once finds the group.
        // SAFETY: `setpgid` takes integers and touches no memory of ours; `pid` is the child
        // just forked, not yet reaped, so it can't name another process.
        unsafe { setpgid(pid, pid) };
        drop(theirs);
        ours.set_nonblocking(true).map_err(|error| format!("the test's output socket: {}", error))?; // alloc: error path
        Ok(Child { id: id, pid: pid, output: ours, bytes: bytes, cut: false, open: true, status: None, exit: None, region: region })
    }

    fn drain(child: &mut Child) {
        let mut buffer = [0u8; 1 << 14];
        while child.open {
            match child.output.read(&mut buffer) {
                Ok(0) => child.open = false,
                Ok(n) => {
                    let room = LONGEST_OUTPUT.saturating_sub(child.bytes.len());
                    if n > room {
                        child.cut = true;
                    }
                    child.bytes.extend_from_slice(&buffer[..n.min(room)]);
                }
                Err(ref error) if error.kind() == ::std::io::ErrorKind::WouldBlock => break,
                Err(ref error) if error.kind() == ::std::io::ErrorKind::Interrupted => {}
                Err(_) => child.open = false,
            }
        }
    }

    fn reap(children: &mut [Child]) {
        loop {
            let mut status: Int = 0;
            // SAFETY: `status` is a live `int` the call writes once; `WNOHANG` keeps it from
            // blocking the server.
            let pid = unsafe { waitpid(-1, &mut status, WNOHANG) };
            if pid <= 0 {
                return;
            }
            if let Some(child) = children.iter_mut().find(|it| it.pid == pid) {
                child.status = Some(status);
            }
        }
    }

    fn end_of(status: Int) -> End {
        match status & 0x7f {
            0 => End::Exited((status >> 8) & 0xff),
            signal => End::Signaled(signal),
        }
    }

    fn kill_group(pid: Int) {
        // SAFETY: `kill` takes integers and touches no memory of ours. `pid` is a child not yet
        // reaped (callers pass only those with no status), so neither it nor its group can
        // have been reused for another process; a reaped one could, and this would kill it.
        unsafe { kill(-pid, SIGKILL) };
        // SAFETY: as above.
        unsafe { kill(pid, SIGKILL) };
    }

    fn kill_all(children: &mut Vec<Child>) {
        for child in children.iter().filter(|it| it.status.is_none()) {
            kill_group(child.pid);
        }
        for child in children.iter().filter(|it| it.status.is_none()) {
            let mut status: Int = 0;
            // SAFETY: `status` is a live `int` the call writes once; `child.pid` is a child of
            // this process just killed, so the wait ends.
            unsafe { waitpid(child.pid, &mut status, 0) };
        }
        children.clear();
    }

    /// Ends the child, as a function of `()` so a panic hook can end with it.
    fn exit_now(code: Int) {
        // SAFETY: `_exit` ends the process at once; it runs in a forked child (or its panic
        // hook), whose state nothing reads after, and runs no destructor or `atexit` handler.
        unsafe { _exit(code) }
    }

    /// Tells the sandbox which test this thread runs. Outside a sandbox nothing sees the call,
    /// which fails on a path that is not there and changes nothing.
    fn mark(name: &str) {
        let mut path = Vec::with_capacity(MARK.len() + name.len() + 1);
        path.extend_from_slice(MARK.as_bytes());
        path.extend_from_slice(name.as_bytes());
        path.push(0);
        // SAFETY: `path` is live across the call and ends with the nul pushed above, so
        // `access` reads within it; without the nul it would read past the buffer.
        unsafe { access(path.as_ptr() as *const ::core::ffi::c_char, 0) };
    }

    #[derive(Clone, Copy)]
    enum Panics {
        No,
        Yes,
        WithMessage(&'static str),
    }

    enum Body {
        Test(fn() -> Result<(), String>),
        Bench(fn(&mut test::Bencher) -> Result<(), String>),
    }

    enum Ran<'a> {
        Returned,
        /// The test returned an error: libtest judges its message as a panic's.
        Errored(&'a str),
        Panicked(Option<&'a str>),
    }

    // dyn: a panic's payload, read once per test that panicked.
    fn payload_str(payload: &(dyn ::std::any::Any + Send)) -> Option<&str> {
        if let Some(text) = payload.downcast_ref::<String>() {
            return Some(text.as_str());
        }
        payload.downcast_ref::<&'static str>().map(|text| *text)
    }

    /// The exit code of a test that ran as `ran`: 0 for a pass as libtest's `calc_result`
    /// has it, 1 for a failure, with why on standard error (the test's output).
    fn judge(ran: Ran, panics: &Panics) -> Int {
        let payload = match ran {
            Ran::Returned => match *panics {
                Panics::No => return 0,
                _ => {
                    eprintln!("note: test did not panic as expected");
                    return 1;
                }
            },
            Ran::Errored(message) => {
                if let Panics::No = *panics {
                    eprintln!("{}", message);
                    return 1;
                }
                Some(message)
            }
            Ran::Panicked(payload) => payload,
        };
        match *panics {
            Panics::No => 1,
            Panics::Yes => 0,
            Panics::WithMessage(expected) => match payload {
                Some(message) if message.contains(expected) => 0,
                Some(message) => {
                    eprintln!(
                        "note: panic did not contain expected string\n      panic message: {:?}\n expected substring: {:?}",
                        message, expected
                    );
                    1
                }
                None => {
                    eprintln!(
                        "note: expected panic with string value,\n found non-string value\n     expected substring: {:?}",
                        expected
                    );
                    1
                }
            },
        }
    }

    /// Runs one test in this child as libtest runs it: on a thread named after it, judged by
    /// its `should_panic`. The exit code [`judge`] gives.
    fn run(test: &'static test::TestDescAndFn) -> Int {
        let name = test.desc.name.as_slice();
        mark(name);
        let panics = match test.desc.should_panic {
            test::ShouldPanic::No => Panics::No,
            test::ShouldPanic::Yes => Panics::Yes,
            test::ShouldPanic::YesWithMessage(message) => Panics::WithMessage(message),
        };
        let body = match test.testfn {
            test::StaticTestFn(f) => Body::Test(f),
            test::StaticBenchFn(f) => Body::Bench(f),
            _ => {
                eprintln!("blaze's fork server runs static tests only, and `{}` is not one", name);
                return 1;
            }
        };
        if cfg!(panic = "abort") {
            // No unwinding to catch: the first panic is judged where it happens.
            let judged = panics;
            let default = ::std::panic::take_hook();
            ::std::panic::set_hook(Box::new(move |info| { // alloc: in the test's child, after its fork: libtest's own hook and thread, nothing of the server's
                default(info);
                let code = judge(Ran::Panicked(payload_str(info.payload())), &judged);
                let _ = ::std::io::stderr().flush();
                exit_now(code);
            }));
        }
        let thread = ::std::thread::Builder::new().name(name.to_owned()).spawn(move || match body { // alloc: in the test's child, after its fork: libtest's own hook and thread, nothing of the server's
            Body::Test(f) => f(),
            Body::Bench(f) => test::bench::run_once(f),
        });
        let handle = match thread {
            Ok(handle) => handle,
            Err(error) => {
                eprintln!("blaze's fork server: the test's thread could not start: {}", error);
                return 1;
            }
        };
        let code = match handle.join() {
            Ok(Ok(())) => judge(Ran::Returned, &panics),
            Ok(Err(message)) => judge(Ran::Errored(&message), &panics),
            Err(payload) => judge(Ran::Panicked(payload_str(&*payload)), &panics),
        };
        mark(name);
        code
    }
}
