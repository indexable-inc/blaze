// Rung 7c: the owner's hello world (/Volumes/Projects/tmp/hello-world, edition blaze-2026.11)
// with no dependency: `main` is his three lines exactly, and what it names from bstd's prelude
// (`Cx`, `Io`, `Out`, `Error`, `Result`, `Ok`) is declared here at its smallest. The shapes are
// bstd's (crates/blaze-std/src: `Io::need_out(&self) -> Result<Out, Missing>` in io.rs,
// `Out::line(&self, cx: &Cx, ..) -> Result<(), OutError>` in out.rs, `Error` with `From` from
// each error in entry.rs), so each `?` converts one error into another, as his does.
//
// `?` is lowered as rustc lowers it, against the lang items the real `core` declares (the same
// declarations, cut to what this program uses), so the rung's desugaring is the one rung 8 runs:
//
//     match Try::branch(x) {
//         ControlFlow::Continue(v) => v,
//         ControlFlow::Break(r) => return FromResidual::from_residual(r),
//     }
//
// It needs: enums with payloads and `match` on them, generic traits with associated types and a
// default type parameter (`FromResidual<R = <Self as Try>::Residual>`), a supertrait, an
// uninhabited residual (`Result<Infallible, E>`, matched by its `Err` arm alone), `From` between
// the errors, `&str` with its length (`transmute` to its two words, as `core` reads the
// metadata), and method calls on a value, a reference and a temporary.
//
// The entry: bstd's `main` attribute makes the C `main` that builds a `Cx` and an `Io` and
// calls this `main`; here it is written out (`start`, exported as `main`).
#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = "pointee_sized"]
pub trait PointeeSized {}

#[lang = "meta_sized"]
pub trait MetaSized: PointeeSized {}

#[lang = "sized"]
pub trait Sized: MetaSized {}

#[lang = "copy"]
pub trait Copy {}

impl Copy for i32 {}
impl Copy for usize {}
impl<T: ?Sized> Copy for &T {}

#[rustc_intrinsic]
unsafe fn transmute<T, U>(x: T) -> U;

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

// What `core` declares for `?` (core::ops::try_trait, core::ops::control_flow,
// core::convert, core::result), cut to this program.

pub enum Infallible {}

pub enum ControlFlow<B, C> {
    #[lang = "Continue"]
    Continue(C),
    #[lang = "Break"]
    Break(B),
}

#[lang = "Try"]
pub trait Try: FromResidual {
    type Output;
    type Residual;
    #[lang = "from_output"]
    fn from_output(output: Self::Output) -> Self;
    #[lang = "branch"]
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
}

#[lang = "FromResidual"]
pub trait FromResidual<R = <Self as Try>::Residual> {
    #[lang = "from_residual"]
    fn from_residual(residual: R) -> Self;
}

pub trait From<T>: Sized {
    fn from(value: T) -> Self;
}

// core's blanket (library/core/src/convert/mod.rs): without it `impl Try for Result<T, E>` is
// not well formed (its supertrait `FromResidual<Result<Infallible, E>>` needs `E: From<E>`), and
// rustc refuses the crate; a `?` whose error type is the function's own takes it.
impl<T> From<T> for T {
    fn from(t: T) -> T {
        t
    }
}

pub enum Result<T, E> {
    #[lang = "Ok"]
    Ok(T),
    #[lang = "Err"]
    Err(E),
}

use Result::{Err, Ok};

impl<T, E> Try for Result<T, E> {
    type Output = T;
    type Residual = Result<Infallible, E>;

    fn from_output(output: T) -> Self {
        Ok(output)
    }

    fn branch(self) -> ControlFlow<Result<Infallible, E>, T> {
        match self {
            Ok(v) => ControlFlow::Continue(v),
            Err(e) => ControlFlow::Break(Err(e)),
        }
    }
}

impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F> {
    fn from_residual(residual: Result<Infallible, E>) -> Self {
        match residual {
            Err(e) => Err(From::from(e)),
        }
    }
}

// bstd's prelude, cut to what `main` uses.

/// The context every effect takes (bstd's `Cx`).
pub struct Cx {
    depth: u32,
}

/// The process's capabilities (bstd's `Io`): here, standard output's descriptor.
pub struct Io {
    out: i32,
}

/// A stream (bstd's `Out`).
pub struct Out {
    fd: i32,
}

/// The process has no such stream (bstd's `Missing`).
pub struct Missing;

/// A write failed (bstd's `OutError`).
pub struct OutError;

/// What `main` returns on failure (bstd's `Error`): every error converts into it.
pub struct Error {
    code: i32,
}

impl From<Missing> for Error {
    fn from(_: Missing) -> Error {
        Error { code: 2 }
    }
}

impl From<OutError> for Error {
    fn from(_: OutError) -> Error {
        Error { code: 3 }
    }
}

impl Io {
    pub fn need_out(&self) -> Result<Out, Missing> {
        if self.out < 0 { Err(Missing) } else { Ok(Out { fd: self.out }) }
    }
}

impl Out {
    /// `text` and a line feed.
    pub fn line(&self, cx: &Cx, text: &str) -> Result<(), OutError> {
        let _ = cx.depth;
        // A `&str` is its address and its length, as `core`'s `str::len` reads it.
        let (ptr, len): (*const u8, usize) = unsafe { transmute(text) };
        let wrote = unsafe { write(self.fd, ptr, len) };
        if wrote != len as isize {
            return Err(OutError);
        }
        let lf = unsafe { write(self.fd, b"\n" as *const u8, 1) };
        if lf != 1 { Err(OutError) } else { Ok(()) }
    }
}

// The owner's program, unchanged.

fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, "Hello, world!")?;
    Ok(())
}

// What bstd's entry makes of it.

#[unsafe(export_name = "main")]
pub extern "C" fn start() -> i32 {
    let cx = Cx { depth: 0 };
    let io = Io { out: 1 };
    match main(&cx, io) {
        Ok(()) => 0,
        Err(error) => error.code,
    }
}
