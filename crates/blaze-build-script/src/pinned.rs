//! The try-compiles tempo's build scripts make, each answered for the pinned nightly by
//! reading its library and the language it implements, with the reason; never by running rustc
//! (rule 1). A script asking a question not here is stopped with
//! `CannotAnswerYet`, which quotes the kept arguments and the snippet to add as a row.
//!
//! The snippets are the packages' bytes exactly: a row whose snippet differs by one byte has
//! another key and answers nothing.

use crate::probe::Known;

/// thiserror's `build/probe.rs` at 1.0.69 (std's `Error`, `std::error::Request`).
const THISERROR_1_PROBE: &str = r#"// This code exercises the surface area that we expect of the Error generic
// member access API. If the current toolchain is able to compile it, then
// thiserror is able to provide backtrace support.

#![feature(error_generic_member_access)]

use core::fmt::{self, Debug, Display};
use std::error::{Error, Request};

struct MyError(Thing);
struct Thing;

impl Debug for MyError {
    fn fmt(&self, _formatter: &mut fmt::Formatter) -> fmt::Result {
        unimplemented!()
    }
}

impl Display for MyError {
    fn fmt(&self, _formatter: &mut fmt::Formatter) -> fmt::Result {
        unimplemented!()
    }
}

impl Error for MyError {
    fn provide<'a>(&'a self, request: &mut Request<'a>) {
        request.provide_ref(&self.0);
    }
}

// Include in sccache cache key.
const _: Option<&str> = option_env!("RUSTC_BOOTSTRAP");
"#;

/// thiserror's `build/probe.rs` at 2.0.21 (`no_std`, `core::error::Request`).
pub(crate) const THISERROR_2_PROBE: &str = r#"// This code exercises the surface area that we expect of the Error generic
// member access API. If the current toolchain is able to compile it, then
// thiserror is able to provide backtrace support.

#![no_std]
#![feature(error_generic_member_access)]

use core::error::{Error, Request};
use core::fmt::{self, Debug, Display};

struct MyError(Thing);
struct Thing;

impl Debug for MyError {
    fn fmt(&self, _formatter: &mut fmt::Formatter) -> fmt::Result {
        unimplemented!()
    }
}

impl Display for MyError {
    fn fmt(&self, _formatter: &mut fmt::Formatter) -> fmt::Result {
        unimplemented!()
    }
}

impl Error for MyError {
    fn provide<'a>(&'a self, request: &mut Request<'a>) {
        request.provide_ref(&self.0);
    }
}

// Include in sccache cache key.
const _: Option<&str> = option_env!("RUSTC_BOOTSTRAP");
"#;

/// Why both thiserror probes pass: the pinned nightly-2026-10-27's library.
const ERROR_GENERIC_MEMBER_ACCESS: &str = "nightly-2026-10-27's core/src/error.rs has `pub struct Request<'a>` (line 588), \
     `Error::provide` (line 260) and `Request::provide_ref` (line 687), all under the unstable feature \
     `error_generic_member_access`, which a nightly accepts; std/src/error.rs re-exports `Request` (line 7)";

/// eyre 0.6.14's `build.rs` probe, written to `OUT_DIR` and compiled from there.
const EYRE_PROBE: &str = "\n    #![feature(error_generic_member_access)]\n    #![allow(dead_code)]\n\n    use std::error::{Error, Request};\n    use std::fmt::{self, Display};\n\n    #[derive(Debug)]\n    struct E { \n        backtrace: MyBacktrace,\n    }\n\n    #[derive(Debug)]\n    struct MyBacktrace;\n\n    impl Display for E {\n        fn fmt(&self, _formatter: &mut fmt::Formatter) -> fmt::Result {\n            unimplemented!()\n        }\n    }\n\n    impl Error for E {\n        fn provide<'a>(&'a self, request: &mut Request<'a>) {\n            request\n                .provide_ref::<MyBacktrace>(&self.backtrace);\n        }\n    }\n";

/// anyhow 1.0.104's `src/nightly.rs`, compiled with `--cfg anyhow_build_probe`.
const ANYHOW_PROBE: &str = r#"// This code exercises the surface area that we expect of the Error generic
// member access API. If the current toolchain is able to compile it, then
// anyhow is able to provide backtrace support.

#![cfg_attr(anyhow_build_probe, feature(error_generic_member_access))]

use core::error::{self, Error};
use std::backtrace::Backtrace;

pub use core::error::Request;

#[cfg(anyhow_build_probe)]
const _: () = {
    use core::fmt::{self, Debug, Display};

    struct MyError(Backtrace);

    impl Debug for MyError {
        fn fmt(&self, _formatter: &mut fmt::Formatter) -> fmt::Result {
            unimplemented!()
        }
    }

    impl Display for MyError {
        fn fmt(&self, _formatter: &mut fmt::Formatter) -> fmt::Result {
            unimplemented!()
        }
    }

    impl Error for MyError {
        fn provide<'a>(&'a self, request: &mut Request<'a>) {
            provide_ref_backtrace(request, &self.0);
        }
    }
};

// Include in sccache cache key.
#[cfg(anyhow_build_probe)]
const _: Option<&str> = option_env!("RUSTC_BOOTSTRAP");

pub fn request_ref_backtrace(err: &dyn Error) -> Option<&Backtrace> {
    request_ref::<Backtrace>(err)
}

fn request_ref<'a, T>(err: &'a (impl Error + ?Sized)) -> Option<&'a T>
where
    T: 'static + ?Sized,
{
    error::request_ref::<T>(err)
}

pub fn provide_ref_backtrace<'a>(request: &mut Request<'a>, backtrace: &'a Backtrace) {
    Request::provide_ref(request, backtrace);
}

pub fn provide<'a>(err: &'a (impl Error + ?Sized), request: &mut Request<'a>) {
    Error::provide(err, request);
}
"#;

/// proc-macro2 1.0.107's `src/probe/proc_macro_span.rs`.
const PROC_MACRO2_SPAN: &str = r#"// This code exercises the surface area that we expect of Span's unstable API.
// If the current toolchain is able to compile it, then proc-macro2 is able to
// offer these APIs too.

#![cfg_attr(procmacro2_build_probe, no_std)]
#![cfg_attr(procmacro2_build_probe, feature(proc_macro_span))]

extern crate alloc;
extern crate proc_macro;
extern crate std;

use alloc::string::String;
use core::ops::{Range, RangeBounds};
use proc_macro::{Literal, Span};
use std::path::PathBuf;

pub fn byte_range(this: &Span) -> Range<usize> {
    this.byte_range()
}

pub fn start(this: &Span) -> Span {
    this.start()
}

pub fn end(this: &Span) -> Span {
    this.end()
}

pub fn line(this: &Span) -> usize {
    this.line()
}

pub fn column(this: &Span) -> usize {
    this.column()
}

pub fn file(this: &Span) -> String {
    this.file()
}

pub fn local_file(this: &Span) -> Option<PathBuf> {
    this.local_file()
}

pub fn join(this: &Span, other: Span) -> Option<Span> {
    this.join(other)
}

pub fn subspan<R: RangeBounds<usize>>(this: &Literal, range: R) -> Option<Span> {
    this.subspan(range)
}

// Include in sccache cache key.
#[cfg(procmacro2_build_probe)]
const _: Option<&str> = option_env!("RUSTC_BOOTSTRAP");
"#;

/// proc-macro2 1.0.107's `src/probe/proc_macro_span_file.rs`.
const PROC_MACRO2_SPAN_FILE: &str = r#"// The subset of Span's API stabilized in Rust 1.88.

#![cfg_attr(procmacro2_build_probe, no_std)]

extern crate alloc;
extern crate proc_macro;
extern crate std;

use alloc::string::String;
use proc_macro::Span;
use std::path::PathBuf;

pub fn file(this: &Span) -> String {
    this.file()
}

pub fn local_file(this: &Span) -> Option<PathBuf> {
    this.local_file()
}
"#;

/// proc-macro2 1.0.107's `src/probe/proc_macro_span_location.rs`.
const PROC_MACRO2_SPAN_LOCATION: &str = r#"// The subset of Span's API stabilized in Rust 1.88.

#![cfg_attr(procmacro2_build_probe, no_std)]

extern crate alloc;
extern crate proc_macro;
extern crate std;

use proc_macro::Span;

pub fn start(this: &Span) -> Span {
    this.start()
}

pub fn end(this: &Span) -> Span {
    this.end()
}

pub fn line(this: &Span) -> usize {
    this.line()
}

pub fn column(this: &Span) -> usize {
    this.column()
}
"#;

/// The arguments dtolnay's probes (anyhow, thiserror, proc-macro2) give besides their own:
/// metadata only (`--emit=dep-info,metadata`; `--crate-name` and `--out-dir` say only where
/// the output goes; the probe file is the snippet).
const DTOLNAY_EMIT: &str = "--emit=dep-info,metadata";

/// proc-macro2 1.0.107's probe arguments.
const PROC_MACRO2: &[&str] = &["--cfg=procmacro2_build_probe", "--edition=2021", "--crate-type=lib", "--cap-lints=allow", DTOLNAY_EMIT];

/// autocfg 1.5.1's arguments: `--emit=llvm-ir`, so code is generated (its `--crate-name` and
/// `--out-dir` say where; the snippet comes on stdin).
const AUTOCFG: &[&str] = &["--crate-type=lib", "--emit=llvm-ir"];

/// rustix 1.1.5's `can_compile` arguments (`-o` says where; stdin).
const RUSTIX: &[&str] = &["--crate-type=rlib", "--emit=metadata"];

/// proc-macro2 1.0.94's `build/probe.rs`, exactly (turso's macro side; its build.rs passes no
/// `--cfg`).
const PROC_MACRO2_094_SPAN: &str = "// This code exercises the surface area that we expect of Span's unstable API.\n// If the current toolchain is able to compile it, then proc-macro2 is able to\n// offer these APIs too.\n\n#![feature(proc_macro_span)]\n\nextern crate proc_macro;\n\nuse core::ops::{Range, RangeBounds};\nuse proc_macro::{Literal, Span};\n\npub fn byte_range(this: &Span) -> Range<usize> {\n    this.byte_range()\n}\n\npub fn start(this: &Span) -> Span {\n    this.start()\n}\n\npub fn end(this: &Span) -> Span {\n    this.end()\n}\n\npub fn line(this: &Span) -> usize {\n    this.line()\n}\n\npub fn column(this: &Span) -> usize {\n    this.column()\n}\n\npub fn join(this: &Span, other: Span) -> Option<Span> {\n    this.join(other)\n}\n\npub fn subspan<R: RangeBounds<usize>>(this: &Literal, range: R) -> Option<Span> {\n    this.subspan(range)\n}\n\n// Include in sccache cache key.\nconst _: Option<&str> = option_env!(\"RUSTC_BOOTSTRAP\");\n";

/// proc-macro2 1.0.94's probe arguments (no `--cfg=procmacro2_build_probe`, unlike 1.0.107).
const PROC_MACRO2_094: &[&str] = &["--edition=2021", "--crate-type=lib", "--cap-lints=allow", DTOLNAY_EMIT];

/// The crates most rows rest on.
const STD: &[&str] = &["core", "std"];
const CORE: &[&str] = &["core"];
const PROC_MACRO: &[&str] = &["alloc", "core", "proc_macro", "std"];

/// The rows for tempo, made for each side's target (`probe::pinned_rows`): no row's reason
/// rests on a target-gated item (none of the items named is under a `#[cfg(target_..)]`), so
/// each holds for every target whose toolchain ships its [`Known::needs`]: the host
/// (`aarch64-apple-darwin`) and the macro side's `wasm32-wasip1`.
///
/// Only what blaze cannot decide yet stays: every row here is a snippet with items
/// (`probe_ask::Open::Items`; anyhow's opens with an inner `cfg_attr` over them,
/// `Open::Attribute`), and each is removed by the same piece, the item probe (a probe
/// task that checks the snippet in a world of no dependencies but the side's sysroot crates,
/// by blaze's own front end with the library's stability gates; the lane-BS report's spec). A
/// snippet of inner attributes only (the empty crate, `#![no_std]`, rustix's
/// `#![feature(..)]` probes) is decided by `probe_ask::decide` from the language list and the
/// library listings, so it has no row (`probe_ask_tests::pinned_rows_read_as_try_compiles`
/// fails on one).
pub const TEMPO: &[Known] = &[
    Known {
        probe: "anyhow 1.0.104: error_generic_member_access with std::backtrace (src/nightly.rs)",
        args: &["--cfg=anyhow_build_probe", "--edition=2018", "--crate-type=lib", "--cap-lints=allow", DTOLNAY_EMIT],
        snippet: ANYHOW_PROBE,
        accepted: true,
        why: "as thiserror's: core/src/error.rs `Request` (588), `Error::provide` (260), `Request::provide_ref` (687) and `request_ref` (473) under error_generic_member_access, which the probe's cfg_attr turns on; std/src/backtrace.rs is #![stable(feature = \"backtrace\", since = \"1.65.0\")] for every std target (unsupported platforms capture nothing)",
        needs: STD,
    },
    Known {
        probe: "proc-macro2 1.0.107: proc_macro_span (src/probe/proc_macro_span.rs)",
        args: PROC_MACRO2,
        snippet: PROC_MACRO2_SPAN,
        accepted: true,
        why: "proc_macro/src/lib.rs: Span::byte_range (661), Span::join (716) and Literal::subspan (1584) under the unstable proc_macro_span, which the probe's cfg_attr turns on; start/end/line/column (667-689) stable as proc_macro_span_location and file/local_file (698, 708) as proc_macro_span_file, since 1.88.0",
        needs: PROC_MACRO,
    },
    Known {
        probe: "proc-macro2 1.0.107: proc_macro_span_file (src/probe/proc_macro_span_file.rs)",
        args: PROC_MACRO2,
        snippet: PROC_MACRO2_SPAN_FILE,
        accepted: true,
        why: "proc_macro/src/lib.rs: Span::file (698) and Span::local_file (708), #[stable(feature = \"proc_macro_span_file\", since = \"1.88.0\")]",
        needs: PROC_MACRO,
    },
    Known {
        probe: "proc-macro2 1.0.107: proc_macro_span_location (src/probe/proc_macro_span_location.rs)",
        args: PROC_MACRO2,
        snippet: PROC_MACRO2_SPAN_LOCATION,
        accepted: true,
        why: "proc_macro/src/lib.rs: Span::start (667), end (673), line (681), column (689), #[stable(feature = \"proc_macro_span_location\", since = \"1.88.0\")]",
        needs: PROC_MACRO,
    },
    Known {
        probe: "eyre 0.6.14 (autocfg): `std::panic::Location::caller` (track_caller)",
        args: AUTOCFG,
        snippet: "pub fn probe() { let _ = std::panic::Location::caller; }",
        accepted: true,
        why: "core/src/panic/location.rs:184 `pub const fn caller`, #[stable(feature = \"track_caller\", since = \"1.46.0\")], re-exported as std::panic::Location",
        needs: STD,
    },
    Known {
        probe: "num-traits 0.2.19 (autocfg): `f64::total_cmp`",
        args: AUTOCFG,
        snippet: "pub fn probe() { let _ = 1f64.total_cmp(&2f64); }",
        accepted: true,
        why: "core/src/num/f64.rs:1578 `pub const fn total_cmp`, #[stable(feature = \"total_cmp\", since = \"1.62.0\")]",
        needs: STD,
    },
    Known {
        probe: "eyre 0.6.14: error_generic_member_access (std::error::Request, provide_ref)",
        args: &["--edition=2018", "--crate-type=lib", "--emit=metadata"],
        snippet: EYRE_PROBE,
        accepted: true,
        why: ERROR_GENERIC_MEMBER_ACCESS,
        needs: STD,
    },
    Known {
        probe: "rustix 1.1.5: the diagnostic attribute namespace",
        args: RUSTIX,
        snippet: "#[diagnostic::on_unimplemented()] trait Foo {}\n",
        accepted: true,
        why: "the `diagnostic` namespace is stable (core uses #[diagnostic::on_unimplemented(..)] at core/src/cmp.rs:247 and core/src/lib.rs enables no feature for it); empty arguments are a lint warning, and rustix passes no -D",
        needs: STD,
    },
    Known {
        probe: "rustix 1.1.5: `LowerExp` for `NonZeroI32`",
        args: RUSTIX,
        snippet: "fn a(x: &core::num::NonZeroI32, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { core::fmt::LowerExp::fmt(x, f) }\n",
        accepted: true,
        why: "core/src/num/nonzero.rs:145 implements LowerExp for NonZero<T> under #[stable(feature = \"nonzero_fmt_exp\", since = \"1.84.0\")]; NonZeroI32 is the stable alias of NonZero<i32>",
        needs: STD,
    },
    Known {
        probe: "thiserror 1.0.69: error_generic_member_access (std)",
        args: &["--edition=2018", "--crate-type=lib", "--cap-lints=allow", DTOLNAY_EMIT],
        snippet: THISERROR_1_PROBE,
        accepted: true,
        why: ERROR_GENERIC_MEMBER_ACCESS,
        needs: STD,
    },
    Known {
        probe: "thiserror 2.0.21: error_generic_member_access (core, no_std)",
        args: &["--edition=2018", "--crate-type=lib", "--cap-lints=allow", DTOLNAY_EMIT],
        snippet: THISERROR_2_PROBE,
        accepted: true,
        why: ERROR_GENERIC_MEMBER_ACCESS,
        needs: CORE,
    },
    // turso 2026-11-21 (reports/turso/c1647/check.out:19-26): the probes tempo's versions do
    // not make. Each read against nightly-2026-10-27's library (rust-src of the pinned
    // toolchain, `library/`).
    // Read against library/core/src/ptr/const_ptr.rs:611-615. rustix 0.38.44 build.rs:178 and
    // 1.0.7 build.rs:183 ask it (one key: same line, snippet and target).
    Known {
        probe: "rustix 0.38.44 and 1.0.7: `offset_from` in a const fn (build.rs:178, :183)",
        args: RUSTIX,
        snippet: "const unsafe fn foo(p: *const u8) -> isize { p.offset_from(p) }\n",
        accepted: true,
        why: "core/src/ptr/const_ptr.rs:611-615 `pub const unsafe fn offset_from`, #[stable(feature = \"ptr_offset_from\", since = \"1.47.0\")] and #[rustc_const_stable(feature = \"const_ptr_offset_from\", since = \"1.65.0\")]",
        needs: STD,
    },
    // Read against the sysroot's crate list: `std` is one (library/std/src/lib.rs); the row is
    // made only for a target whose sysroot holds it (`needs`). indexmap 1.9.3 build.rs:5.
    Known {
        probe: "indexmap 1.9.3 (autocfg): the sysroot crate `std`",
        args: AUTOCFG,
        snippet: "extern crate std as probe;",
        accepted: true,
        why: "`std` is a crate of the target's sysroot (the row is made only for a target that ships it, `needs`); `extern crate NAME as probe` is autocfg's `probe_sysroot_crate`",
        needs: STD,
    },
    // Read against library/core/src/num/int_macros.rs:2548-2553. bigdecimal 0.4.10 build.rs:28.
    Known {
        probe: "bigdecimal 0.4.10 (autocfg): `i64::unsigned_abs`",
        args: AUTOCFG,
        snippet: "pub fn probe() { let _ = 0i64.unsigned_abs(); }",
        accepted: true,
        why: "core/src/num/int_macros.rs:2548-2553 `pub const fn unsigned_abs`, #[stable(feature = \"unsigned_abs\", since = \"1.51.0\")]",
        needs: STD,
    },
    // Read against library/proc_macro/src/lib.rs:660-661 (byte_range), 666-689 (start, end,
    // line, column), 715-716 (join), 1583-1584 (Literal::subspan).
    Known {
        probe: "proc-macro2 1.0.94: proc_macro_span (build/probe.rs)",
        args: PROC_MACRO2_094,
        snippet: PROC_MACRO2_094_SPAN,
        accepted: true,
        why: "proc_macro/src/lib.rs: Span::byte_range (661), Span::join (716) and Literal::subspan (1584) under the unstable proc_macro_span, which the probe's #![feature] turns on; start/end/line/column (667-689) stable as proc_macro_span_location since 1.88.0",
        needs: PROC_MACRO,
    },
];
