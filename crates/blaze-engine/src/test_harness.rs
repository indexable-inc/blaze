//! The `main` of a test executable, as rustc's `--test` writes it: a description of each
//! `#[test]` and `#[bench]` of the crate for libtest, and `test_main_env_args` over them. blaze adds it to
//! the root file of a crate it builds with `cfg(test)`, as a module of its own, and the
//! program starts there instead of in rustc's: a test added, removed or marked otherwise is
//! a build like another.

use std::fmt::Write as _;

use ide_db::RootDatabase;
use syntax::{
    AstNode as _, AstToken as _,
    ast::{self, HasAttrs as _, HasName as _},
};

/// The module's name in the crate's root.
pub(crate) const MODULE: &str = "__blaze_test_harness";

/// What the module starts with, after the text of the root file.
const MARK: &str = "\n// blaze's test harness, which is not in the file on disk.\n";

/// Where the module, or a proc-macro crate's [`crate::macro_list`] written the same way,
/// starts in `text`.
fn written_at(text: &str) -> Option<usize> {
    [MARK, crate::macro_list::MARK].iter().filter_map(|it| text.find(it)).min()
}

/// The name of the module at the end of `text`, this one or a [`crate::macro_list`].
pub(crate) fn appended(text: &str) -> Option<&'static str> {
    let at = written_at(text)?;
    Some(if text[at..].starts_with(MARK) { MODULE } else { crate::macro_list::MODULE })
}

/// The text of the root file without the module.
pub(crate) fn without(text: &str) -> &str {
    written_at(text).map_or(text, |at| &text[..at])
}

/// `text` read anew from a file, with the module `was` had at its end. The module stays in
/// the crate from one revision to the next, where it is written again for the tests that
/// are there now: one that is gone a moment is another module when it is back.
pub(crate) fn kept(was: &str, text: String) -> String {
    match written_at(was) {
        Some(at) => text + &was[at..],
        None => text,
    }
}

/// blaze's harness for a crate.
pub(crate) struct Harness {
    /// The module, which starts with [`MARK`].
    pub(crate) text: String,
}

/// One test as libtest describes it.
struct Test {
    /// The function from the crate's root, and the name libtest shows: the same path
    /// without `crate::`.
    path: Vec<String>,
    /// A `#[bench]`, which libtest calls with a `Bencher`.
    bench: bool,
    ignore: bool,
    ignore_message: Option<String>,
    should_panic: ShouldPanic,
    /// Where its name is: the file as rustc is told it, and the line and column of the
    /// start and the end, from 1.
    file: String,
    start: (usize, usize),
    end: (usize, usize),
}

#[derive(Debug)]
enum ShouldPanic {
    No,
    Yes,
    WithMessage(String),
}

/// The module for the tests of `krate`, `None` when a test is not one the module can say as
/// rustc's harness does (its source or its name is not there, or its `should_panic` names no
/// string), which is a bug to fix (no-rustc H, #88).
/// `file_name` is what rustc is told a file is called.
pub(crate) fn module(
    db: &RootDatabase,
    krate: hir::Crate,
    functions: impl IntoIterator<Item = hir::Function>,
    file_name: &dyn Fn(vfs::FileId) -> Option<String>,
) -> Option<Harness> {
    use hir::HasSource as _;
    let edition = krate.edition(db);
    let mut tests = Vec::new();
    for function in functions {
        let bench = function.is_bench(db);
        if !bench && !function.is_test(db) {
            continue;
        }
        let Some(path) = path_of(db, function, edition) else { continue };
        let source = function.source(db)?;
        let mut test = Test {
            path,
            bench,
            ignore: false,
            ignore_message: None,
            should_panic: ShouldPanic::No,
            file: String::new(),
            start: (0, 0),
            end: (0, 0),
        };
        // What `cfg_attr` gives for the crate's cfgs is written as if alone.
        let mut metas: Vec<ast::Meta> = source.value.attrs().filter_map(|it| it.meta()).collect();
        metas.reverse();
        let mut attrs = Vec::new();
        while let Some(meta) = metas.pop() {
            let ast::Meta::CfgAttrMeta(cfg_attr) = meta else {
                attrs.push(meta);
                continue;
            };
            let predicate = cfg::CfgExpr::parse_from_ast(cfg_attr.cfg_predicate()?);
            if krate.cfg(db).check(&predicate) != Some(false) {
                let inner: Vec<ast::Meta> = cfg_attr.metas().collect();
                metas.extend(inner.into_iter().rev());
            }
        }
        for meta in attrs {
            let named = |path: Option<ast::Path>| path.map(|it| it.syntax().text().to_string());
            match meta {
                ast::Meta::PathMeta(meta) => match named(meta.path()).as_deref() {
                    Some("ignore") => test.ignore = true,
                    Some("should_panic") => test.should_panic = ShouldPanic::Yes,
                    _ => {}
                },
                ast::Meta::KeyValueMeta(meta) => {
                    let value = meta.expr().and_then(|it| string_of(&it));
                    match named(meta.path()).as_deref() {
                        Some("ignore") => (test.ignore, test.ignore_message) = (true, value),
                        Some("should_panic") => {
                            test.should_panic = ShouldPanic::WithMessage(value?);
                        }
                        _ => {}
                    }
                }
                // rustc reads no message from `ignore` with a list after it, and ignores the
                // test.
                ast::Meta::TokenTreeMeta(meta) => match named(meta.path()).as_deref() {
                    Some("ignore") => test.ignore = true,
                    Some("should_panic") => {
                        test.should_panic =
                            ShouldPanic::WithMessage(expected(&meta.token_tree()?)?);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        let name = source.value.name()?;
        let range = source.with_value(name.syntax()).original_file_range_rooted(db);
        let file = range.file_id.file_id(db);
        test.file = file_name(file)?;
        let lines = ide_db::line_index(db, file);
        let at = |offset| {
            let it = lines.line_col(offset);
            (it.line as usize + 1, it.col as usize + 1)
        };
        (test.start, test.end) = (at(range.range.start()), at(range.range.end()));
        tests.push(test);
    }
    // rustc lists them by name, the joined path libtest shows: `--exact` finds a name by a
    // binary search of that order (`TestListOrder::Sorted`), which the path's own order is
    // not (`a::b` comes after `a0` by name, before it by path).
    tests.sort_by_cached_key(|it| it.path.join("::"));
    let root = krate.root_file(db);
    let test_type = match file_name(root).as_deref().map(std::path::Path::new) {
        Some(it) if it.parent().is_some_and(|it| it.ends_with("src")) => "UnitTest",
        Some(it) if it.parent().is_some_and(|it| it.ends_with("tests")) => "IntegrationTest",
        _ => "Unknown",
    };
    let tested = krate.cfg(db).check_atom(&cfg::CfgAtom::Flag(intern::sym::test));
    Some(Harness { text: text(&tests, test_type, tested) })
}

/// The path of a test from its crate's root, which joined by `::` is the name libtest shows.
/// `None` for a test inside a function, which rustc's harness leaves out too: it can't be
/// named.
fn path_of(
    db: &RootDatabase,
    function: hir::Function,
    edition: syntax::Edition,
) -> Option<Vec<String>> {
    let modules = function.module(db).path_to_root(db);
    let mut path = Vec::new();
    for module in modules.iter().rev().skip(1) {
        path.push(module.name(db)?.display(db, edition).to_string());
    }
    path.push(function.name(db).display(db, edition).to_string());
    Some(path)
}

/// The name libtest shows of a `#[test]` of `krate`, the one `--exact` picks it by.
pub(crate) fn name(
    db: &RootDatabase,
    krate: hir::Crate,
    function: hir::Function,
) -> Option<String> {
    Some(path_of(db, function, krate.edition(db))?.join("::"))
}

/// `tested` is whether the crate has `cfg(test)`, which the module then asks for: a library
/// built with it for its unit tests has its root file in common with the library others link.
fn text(tests: &[Test], test_type: &str, tested: bool) -> String {
    let mut text = String::from(MARK);
    if tested {
        text.push_str("#[cfg(test)]\n");
    }
    text.push_str("#[doc(hidden)]\n#[allow(warnings, clippy::all)]\npub mod ");
    text.push_str(MODULE);
    text.push_str(
        " {\n    extern crate test;\n\n    pub fn main() -> test::ExitCode {\n        \
         const TESTS: &[&test::TestDescAndFn] = &[",
    );
    for at in 0..tests.len() {
        _ = write!(text, "&T{at}, ");
    }
    text.push_str(
        "];\n        if let ::core::option::Option::Some(code) = server::serve(TESTS) {\n            \
         return code;\n        }\n        test::test_main_env_args(TESTS)\n    }\n",
    );
    text.push_str(MARKED);
    text.push_str(&SERVER.replace("{VAR}", blaze_cargo::cargo::forked::VAR));
    for (at, test) in tests.iter().enumerate() {
        let name = test.path.join("::");
        let ignore_message = match &test.ignore_message {
            Some(it) => format!("::core::option::Option::Some({it:?})"),
            None => "::core::option::Option::None".to_owned(),
        };
        let should_panic = match &test.should_panic {
            ShouldPanic::No => "No".to_owned(),
            ShouldPanic::Yes => "Yes".to_owned(),
            ShouldPanic::WithMessage(it) => format!("YesWithMessage({it:?})"),
        };
        let ((start_line, start_col), (end_line, end_col)) = (test.start, test.end);
        let mark = nul_ended(&format!("{}{name}", crate::sandbox::MARK));
        _ = write!(
            text,
            "\n    const T{at}: test::TestDescAndFn = test::TestDescAndFn {{\n        \
             desc: test::TestDesc {{\n            \
             name: test::StaticTestName({name:?}),\n            \
             ignore: {ignore},\n            \
             ignore_message: {ignore_message},\n            \
             source_file: {file:?},\n            \
             start_line: {start_line}usize,\n            \
             start_col: {start_col}usize,\n            \
             end_line: {end_line}usize,\n            \
             end_col: {end_col}usize,\n            \
             should_panic: test::ShouldPanic::{should_panic},\n            \
             compile_fail: false,\n            \
             no_run: false,\n            \
             test_type: test::TestType::{test_type},\n        \
             }},\n        \
             testfn: {testfn},\n    \
             }};\n",
            ignore = test.ignore,
            file = test.file,
            testfn = match test.bench {
                true => format!(
                    "test::StaticBenchFn(|b| {{ let _marked = marked::Marked::on({mark}); \
                     test::assert_test_result(crate::{name}(b)) }})"
                ),
                false => format!(
                    "test::StaticTestFn(|| {{ let _marked = marked::Marked::on({mark}); \
                     test::assert_test_result(crate::{name}()) }})"
                ),
            },
        );
    }
    text.push_str("}\n");
    text
}

/// What gives the sandbox the test a thread runs ([`crate::sandbox::MARK`]): each test is
/// called between `Marked::on` with its marker and the end of what that returns, on its
/// thread, which look up the marker and the marker with no name (an unwinding test's too).
/// Outside a sandbox each is one `access` that finds nothing.
const MARKED: &str = r#"
    mod marked {
        pub struct Marked;

        impl Marked {
            pub fn on(mark: &'static [u8]) -> Marked {
                look(mark);
                Marked
            }
        }

        impl ::core::ops::Drop for Marked {
            fn drop(&mut self) {
                look(b"/.blaze-test/\0")
            }
        }

        #[cfg(unix)]
        fn look(path: &[u8]) {
            unsafe extern "C" {
                fn access(path: *const ::core::ffi::c_char, mode: ::core::ffi::c_int) -> ::core::ffi::c_int;
            }
            // SAFETY: `path` ends with a nul.
            unsafe { access(path.as_ptr().cast(), 0) };
        }

        #[cfg(not(unix))]
        fn look(_: &[u8]) {}
    }
"#;

/// The fork server ([`blaze_cargo::cargo::forked`]): a program of tests that `blaze test`
/// starts with the variable `{VAR}` naming its end of a socket lists its tests, as libtest's
/// own filters pick them from its arguments, and then runs none itself. For each test blaze
/// asks for, with a file its output goes to, it forks a child that runs that one test with
/// libtest (`--exact`, on a thread of its own, as `cargo test` would) and exits; the server
/// says how each child ended. The server never starts a thread, so the fork is sound; one
/// that has threads before it begins (a static constructor's) says so and runs nothing.
///
/// Each child leads a process group of its own, which a kill takes whole, and on Linux dies
/// with the server; blaze going away (its end of the socket closed with children left) kills
/// them all.
///
/// What it is told, 12 bytes each: an operation (1 runs the test at an index of the list,
/// with the output file's descriptor; 2 kills the child of an id), the id blaze gives the run,
/// and the index. What it says, a line each: `t <name>` or `i <name>\t<message>` (ignored)
/// for each test of the list, `e <tests>` at its end with how many the program has, `x <id>
/// <wait status>` as each child ends, and `f <why>` when it can't go on.
const SERVER: &str = r#"
    #[cfg(not(unix))]
    pub mod server {
        use super::test;
        extern crate std;

        /// Elsewhere than Unix blaze runs no fork server (it refuses first); a program told
        /// to be one says it can't.
        pub fn serve(_: &[&test::TestDescAndFn]) -> self::std::option::Option<test::ExitCode> {
            self::std::env::var_os("{VAR}")?;
            self::std::eprintln!("blaze runs a process per test on Linux and macOS only");
            self::std::option::Option::Some(test::ExitCode::from(101u8))
        }
    }

    #[cfg(unix)]
    pub mod server {
        use super::test;
        // The crate's own `std`: a `no_std` crate's tests have none in the extern prelude.
        extern crate std;
        use self::std::os::raw::{c_int, c_void};

        #[cfg(target_os = "linux")]
        type Nfds = self::std::os::raw::c_ulong;
        #[cfg(not(target_os = "linux"))]
        type Nfds = self::std::os::raw::c_uint;
        #[cfg(target_os = "linux")]
        type Len = usize;
        #[cfg(not(target_os = "linux"))]
        type Len = u32;

        #[repr(C)]
        struct PollFd {
            fd: c_int,
            events: i16,
            revents: i16,
        }

        #[repr(C)]
        struct IoVec {
            base: *mut c_void,
            len: usize,
        }

        #[cfg(target_os = "linux")]
        #[repr(C)]
        struct MsgHdr {
            name: *mut c_void,
            name_len: u32,
            iov: *mut IoVec,
            iov_len: usize,
            control: *mut c_void,
            control_len: Len,
            flags: c_int,
        }

        #[cfg(not(target_os = "linux"))]
        #[repr(C)]
        struct MsgHdr {
            name: *mut c_void,
            name_len: u32,
            iov: *mut IoVec,
            iov_len: c_int,
            control: *mut c_void,
            control_len: Len,
            flags: c_int,
        }

        #[repr(C)]
        struct CmsgHdr {
            len: Len,
            level: c_int,
            kind: c_int,
        }

        #[cfg(target_os = "linux")]
        const SOL_SOCKET: c_int = 1;
        #[cfg(not(target_os = "linux"))]
        const SOL_SOCKET: c_int = 0xffff;
        const SCM_RIGHTS: c_int = 1;
        #[cfg(target_os = "linux")]
        const SIGCHLD: c_int = 17;
        #[cfg(not(target_os = "linux"))]
        const SIGCHLD: c_int = 20;
        const SIGKILL: c_int = 9;
        #[cfg(target_os = "linux")]
        const PR_SET_PDEATHSIG: c_int = 1;
        #[cfg(target_os = "linux")]
        const MSG_CTRUNC: c_int = 8;
        #[cfg(not(target_os = "linux"))]
        const MSG_CTRUNC: c_int = 0x20;
        const WNOHANG: c_int = 1;
        const POLLIN: i16 = 1;
        const F_SETFD: c_int = 2;
        const F_GETFL: c_int = 3;
        const F_SETFL: c_int = 4;
        const FD_CLOEXEC: c_int = 1;
        #[cfg(target_os = "linux")]
        const O_NONBLOCK: c_int = 0o4000;
        #[cfg(not(target_os = "linux"))]
        const O_NONBLOCK: c_int = 4;
        const EINTR: i32 = 4;

        unsafe extern "C" {
            fn fork() -> c_int;
            fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
            fn kill(pid: c_int, signal: c_int) -> c_int;
            fn pipe(fds: *mut c_int) -> c_int;
            fn dup2(from: c_int, to: c_int) -> c_int;
            fn close(fd: c_int) -> c_int;
            fn read(fd: c_int, buf: *mut c_void, len: usize) -> isize;
            fn write(fd: c_int, buf: *const c_void, len: usize) -> isize;
            fn poll(fds: *mut PollFd, count: Nfds, timeout: c_int) -> c_int;
            fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> isize;
            fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
            fn signal(signal: c_int, handler: usize) -> usize;
            fn setpgid(pid: c_int, group: c_int) -> c_int;
            #[cfg(target_os = "linux")]
            fn prctl(option: c_int, ...) -> c_int;
            #[cfg(target_os = "macos")]
            fn pthread_is_threaded_np() -> c_int;
        }

        /// The write end of the pipe the handler of `SIGCHLD` wakes the loop with.
        static WAKE: self::std::sync::atomic::AtomicI32 = self::std::sync::atomic::AtomicI32::new(-1);

        extern "C" fn on_child(_: c_int) {
            let byte = 0u8;
            let fd = WAKE.load(self::std::sync::atomic::Ordering::Relaxed);
            // SAFETY: `write` is async-signal-safe; the pipe does not block.
            unsafe { write(fd, &byte as *const u8 as *const c_void, 1) };
        }

        fn interrupted() -> bool {
            self::std::io::Error::last_os_error().raw_os_error() == self::std::option::Option::Some(EINTR)
        }

        fn say(fd: c_int, text: &str) {
            let mut left = text.as_bytes();
            while !left.is_empty() {
                // SAFETY: `left` is that many bytes.
                let wrote = unsafe { write(fd, left.as_ptr() as *const c_void, left.len()) };
                if wrote <= 0 {
                    if wrote < 0 && interrupted() {
                        continue;
                    }
                    return;
                }
                left = &left[wrote as usize..];
            }
        }

        /// The Linux targets whose signal numbers, flags and socket level are not those
        /// above.
        #[cfg(all(target_os = "linux", any(target_arch = "mips", target_arch = "mips64", target_arch = "mips32r6", target_arch = "mips64r6", target_arch = "sparc", target_arch = "sparc64")))]
        fn threaded() -> self::std::option::Option<self::std::string::String> {
            self::std::option::Option::Some(self::std::string::String::from(
                "blaze's fork server does not run on MIPS or SPARC Linux, whose constants differ",
            ))
        }

        /// Why this process can't fork soundly: it has threads.
        #[cfg(all(target_os = "linux", not(any(target_arch = "mips", target_arch = "mips64", target_arch = "mips32r6", target_arch = "mips64r6", target_arch = "sparc", target_arch = "sparc64"))))]
        fn threaded() -> self::std::option::Option<self::std::string::String> {
            match self::std::fs::read_dir("/proc/self/task") {
                self::std::result::Result::Ok(it) => match it.count() {
                    1 => self::std::option::Option::None,
                    n => self::std::option::Option::Some(self::std::format!(
                        "it has {} threads before any test runs (a static constructor started them), and a process with threads can't fork soundly", n
                    )),
                },
                self::std::result::Result::Err(error) => self::std::option::Option::Some(self::std::format!(
                    "it can't tell whether it has threads (`/proc/self/task`: {})", error
                )),
            }
        }

        #[cfg(target_os = "macos")]
        fn threaded() -> self::std::option::Option<self::std::string::String> {
            // SAFETY: no arguments.
            match unsafe { pthread_is_threaded_np() } {
                0 => self::std::option::Option::None,
                _ => self::std::option::Option::Some(self::std::string::String::from(
                    "it started a thread before any test runs (a static constructor's), and on macOS a process that has had threads can't fork soundly"
                )),
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        fn threaded() -> self::std::option::Option<self::std::string::String> {
            self::std::option::Option::Some(self::std::string::String::from("blaze runs a process per test on Linux and macOS only"))
        }

        /// Reads what blaze sent into `pending`, and the descriptors it passed into `fds`:
        /// `Ok(false)` once blaze is gone, and an error when descriptors were lost, which
        /// would give the tests after them each other's output files.
        fn receive(fd: c_int, pending: &mut self::std::vec::Vec<u8>, fds: &mut self::std::vec::Vec<c_int>) -> self::std::result::Result<bool, &'static str> {
            let mut data = [0u8; 4096];
            let mut control = [0u64; 32];
            let mut iov = IoVec { base: data.as_mut_ptr() as *mut c_void, len: data.len() };
            // SAFETY: every field is an integer or a pointer.
            let mut msg: MsgHdr = unsafe { self::std::mem::zeroed() };
            msg.iov = &mut iov;
            msg.iov_len = 1;
            msg.control = control.as_mut_ptr() as *mut c_void;
            msg.control_len = self::std::mem::size_of_val(&control) as Len;
            let got = loop {
                // SAFETY: `msg` points at buffers of the lengths it says.
                let got = unsafe { recvmsg(fd, &mut msg, 0) };
                if got < 0 && interrupted() {
                    continue;
                }
                break got;
            };
            if got <= 0 {
                return self::std::result::Result::Ok(false);
            }
            if msg.flags & MSG_CTRUNC != 0 {
                return self::std::result::Result::Err("descriptors blaze sent were cut off (MSG_CTRUNC)");
            }
            pending.extend_from_slice(&data[..got as usize]);
            let (header, align) = (self::std::mem::size_of::<CmsgHdr>(), self::std::mem::size_of::<Len>());
            let aligned = |it: usize| (it + align - 1) & !(align - 1);
            let base = control.as_ptr() as *const u8;
            let (mut at, end) = (0usize, msg.control_len as usize);
            while at + header <= end {
                // SAFETY: a header is there, aligned as `control` is.
                let head = unsafe { &*(base.add(at) as *const CmsgHdr) };
                let len = head.len as usize;
                if len < header || at + len > end {
                    break;
                }
                if head.level == SOL_SOCKET && head.kind == SCM_RIGHTS {
                    let first = at + aligned(header);
                    for k in 0..(at + len).saturating_sub(first) / 4 {
                        // SAFETY: within the message's data.
                        fds.push(unsafe { self::std::ptr::read_unaligned(base.add(first + 4 * k) as *const c_int) });
                    }
                }
                at += aligned(len);
            }
            self::std::result::Result::Ok(true)
        }

        /// The server, when blaze started this program as one: its code, or the code of
        /// the child that ran a test. `None` when it was not.
        pub fn serve(tests: &[&test::TestDescAndFn]) -> self::std::option::Option<test::ExitCode> {
            let failed = self::std::option::Option::Some(test::ExitCode::from(101u8));
            let given = self::std::env::var_os("{VAR}")?;
            let fd: c_int = match given.to_str().and_then(|it| it.parse().ok()) {
                self::std::option::Option::Some(it) => it,
                self::std::option::Option::None => {
                    self::std::eprintln!("{VAR}={:?} names no descriptor", given);
                    return failed;
                }
            };
            let one_line = |it: &str| it.replace('\n', " ");
            if let self::std::option::Option::Some(why) = threaded() {
                say(fd, &self::std::format!("f {}\n", one_line(&why)));
                return failed;
            }
            // SAFETY: no thread has started (`threaded` said so).
            unsafe {
                self::std::env::remove_var("{VAR}");
                fcntl(fd, F_SETFD, FD_CLOEXEC);
            }
            let args: self::std::vec::Vec<self::std::string::String> = self::std::env::args().collect();
            let opts = match test::test::parse_opts(&args) {
                self::std::option::Option::Some(self::std::result::Result::Ok(it)) => it,
                self::std::option::Option::Some(self::std::result::Result::Err(why)) => {
                    say(fd, &self::std::format!("f {}\n", one_line(&why)));
                    return failed;
                }
                self::std::option::Option::None => {
                    say(fd, "f the harness was asked for its usage\n");
                    return failed;
                }
            };
            let listed = test::filter_tests(&opts, test::TestList::new(tests, test::TestListOrder::Sorted));
            let mut text = self::std::string::String::new();
            for it in &listed {
                let name = it.desc.name.as_slice();
                match it.desc.ignore {
                    true => text.push_str(&self::std::format!("i {}\t{}\n", name, one_line(it.desc.ignore_message.unwrap_or("")))),
                    false => text.push_str(&self::std::format!("t {}\n", name)),
                }
            }
            text.push_str(&self::std::format!("e {}\n", tests.len()));
            say(fd, &text);
            let mut wake = [0 as c_int; 2];
            // SAFETY: two descriptors' room.
            if unsafe { pipe(wake.as_mut_ptr()) } != 0 {
                say(fd, "f no pipe could be made\n");
                return failed;
            }
            // SAFETY: plain calls on descriptors this process owns.
            unsafe {
                for it in wake {
                    fcntl(it, F_SETFD, FD_CLOEXEC);
                    let flags = fcntl(it, F_GETFL);
                    fcntl(it, F_SETFL, flags | O_NONBLOCK);
                }
            }
            WAKE.store(wake[1], self::std::sync::atomic::Ordering::Relaxed);
            // SAFETY: the handler only writes to the pipe.
            unsafe { signal(SIGCHLD, on_child as usize) };
            let mut children: self::std::vec::Vec<(c_int, u32)> = self::std::vec::Vec::new();
            let mut pending: self::std::vec::Vec<u8> = self::std::vec::Vec::new();
            let mut fds: self::std::vec::Vec<c_int> = self::std::vec::Vec::new();
            let mut open = true;
            loop {
                loop {
                    let mut status: c_int = 0;
                    // SAFETY: `status` is an int.
                    let pid = unsafe { waitpid(-1, &mut status, WNOHANG) };
                    if pid <= 0 {
                        break;
                    }
                    if let self::std::option::Option::Some(at) = children.iter().position(|it| it.0 == pid) {
                        let (_, id) = children.swap_remove(at);
                        say(fd, &self::std::format!("x {} {}\n", id, status));
                    }
                }
                if !open && children.is_empty() {
                    return self::std::option::Option::Some(test::ExitCode::SUCCESS);
                }
                let mut polled = [
                    PollFd { fd: if open { fd } else { -1 }, events: POLLIN, revents: 0 },
                    PollFd { fd: wake[0], events: POLLIN, revents: 0 },
                ];
                // SAFETY: two entries.
                if unsafe { poll(polled.as_mut_ptr(), 2, -1) } < 0 {
                    if interrupted() {
                        continue;
                    }
                    say(fd, "f poll failed\n");
                    return failed;
                }
                if polled[1].revents != 0 {
                    let mut drained = [0u8; 64];
                    // SAFETY: into `drained`, which does not block.
                    while unsafe { read(wake[0], drained.as_mut_ptr() as *mut c_void, drained.len()) } > 0 {}
                }
                if polled[0].revents == 0 {
                    continue;
                }
                open = match receive(fd, &mut pending, &mut fds) {
                    self::std::result::Result::Ok(it) => it,
                    self::std::result::Result::Err(why) => {
                        say(fd, &self::std::format!("f {}\n", why));
                        false
                    }
                };
                if !open {
                    // blaze closes its end only once no test of this program runs, so a
                    // child left means blaze went away: its tests are not to outlive it.
                    for it in &children {
                        // SAFETY: children of this process not waited for, and their groups.
                        unsafe {
                            kill(-it.0, SIGKILL);
                            kill(it.0, SIGKILL);
                        }
                    }
                    continue;
                }
                while pending.len() >= 12 {
                    let word = |at: usize| u32::from_ne_bytes([pending[at], pending[at + 1], pending[at + 2], pending[at + 3]]);
                    let (op, id, index) = (word(0), word(4), word(8) as usize);
                    pending.drain(..12);
                    if op == 2 {
                        if let self::std::option::Option::Some(it) = children.iter().find(|it| it.1 == id) {
                            // SAFETY: a child of this process that has not been waited for, and
                            // the group it made, so what the test started goes too.
                            unsafe {
                                kill(-it.0, SIGKILL);
                                kill(it.0, SIGKILL);
                            }
                        }
                        continue;
                    }
                    if op != 1 {
                        say(fd, &self::std::format!("f blaze asked for operation {}\n", op));
                        return failed;
                    }
                    if fds.is_empty() {
                        say(fd, "f a request to run a test came without its output file\n");
                        return failed;
                    }
                    let out = fds.remove(0);
                    if index >= listed.len() {
                        // SAFETY: the descriptor blaze passed for this request.
                        unsafe { close(out) };
                        say(fd, &self::std::format!("x {} -1\n", id));
                        continue;
                    }
                    let name = listed[index].desc.name.as_slice();
                    // SAFETY: this process has one thread.
                    let pid = unsafe { fork() };
                    if pid == 0 {
                        // SAFETY: the child's own descriptors, signal disposition and group.
                        unsafe {
                            setpgid(0, 0);
                            #[cfg(target_os = "linux")]
                            prctl(PR_SET_PDEATHSIG, SIGKILL);
                            signal(SIGCHLD, 0);
                            close(fd);
                            close(wake[0]);
                            close(wake[1]);
                            for it in &fds {
                                close(*it);
                            }
                            dup2(out, 1);
                            dup2(out, 2);
                            close(out);
                        }
                        // The test alone, so libtest's filter can't miss it and pass with none run.
                        let alone: [&test::TestDescAndFn; 1] = match tests.iter().find(|it| it.desc.name.as_slice() == name) {
                            self::std::option::Option::Some(it) => [*it],
                            self::std::option::Option::None => {
                                self::std::eprintln!("blaze: the harness has no test {}", name);
                                return failed;
                            }
                        };
                        let one: self::std::vec::Vec<self::std::string::String> = [
                            args.first().map_or("", |it| it.as_str()),
                            name,
                            "--exact",
                            "--include-ignored",
                            "--test-threads=1",
                            "--nocapture",
                            "--color=never",
                            "-Z",
                            "unstable-options",
                            "--format",
                            "json",
                        ]
                        .iter()
                        .map(|it| self::std::string::String::from(*it))
                        .collect();
                        return self::std::option::Option::Some(test::test_main(&one, &alone));
                    }
                    // SAFETY: the child has its copy.
                    unsafe { close(out) };
                    match pid {
                        p if p < 0 => say(fd, &self::std::format!("x {} -1\n", id)),
                        p => children.push((p, id)),
                    }
                }
            }
        }
    }
"#;

/// `text` as a byte string literal with a nul at its end, which every edition reads.
fn nul_ended(text: &str) -> String {
    let mut out = String::from("b\"");
    for &byte in text.as_bytes() {
        match byte {
            b'"' | b'\\' => out.extend(['\\', byte as char]),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out.push_str("\\0\"");
    out
}

/// The value of a string literal.
fn string_of(expr: &ast::Expr) -> Option<String> {
    let ast::Expr::Literal(literal) = expr else { return None };
    let ast::LiteralKind::String(string) = literal.kind() else { return None };
    Some(string.value().ok()?.into_owned())
}

/// The message of `should_panic(expected = "...")`. `None` for any other argument: rustc
/// takes no other.
fn expected(tree: &ast::TokenTree) -> Option<String> {
    let tokens: Vec<_> = tree
        .syntax()
        .children_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| !it.kind().is_trivia())
        .collect();
    match tokens.as_slice() {
        [open, name, eq, value, close]
            if open.text() == "("
                && name.text() == "expected"
                && eq.text() == "="
                && close.text() == ")" =>
        {
            Some(ast::String::cast(value.clone())?.value().ok()?.into_owned())
        }
        _ => None,
    }
}
