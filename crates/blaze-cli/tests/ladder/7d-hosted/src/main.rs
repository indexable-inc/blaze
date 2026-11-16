// Rung 7d: rung 7c's program, hosted. Its `main` is the owner's three lines, as in 7c, but the
// program calls no C `write`: it is started by `blaze run` through bstd's hosted entry,
// `bstd_hosted_start(table)`, and every effect goes through the host table blaze hands it
// (crates/blaze-std/src/wire.rs, wire version 1). `Out::line` copies its bytes into the host's
// exchange buffer and calls the table's `write` with their range there.
//
// The table is declared here with wire.rs's layout field for field (64-bit offsets in the
// comments); `*mut u8` stands for `*mut c_void`, the same pointer.
//
// It needs, beyond 7c: a `#[repr(C)]` struct, fn pointer fields and calls through them, a
// `#[repr(C)]` 16-byte result returned in two registers (`Answer`), raw pointers read and
// written at an offset, and `as` casts between pointers and `usize`.
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
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for usize {}
impl<T: ?Sized> Copy for &T {}
impl<T: ?Sized> Copy for *const T {}
impl<T: ?Sized> Copy for *mut T {}

#[rustc_intrinsic]
unsafe fn transmute<T, U>(x: T) -> U;

// What `core` declares for `?`, as in rung 7c.

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

// The host table (crates/blaze-std/src/wire.rs), wire version 1.

/// A host call's answer: a code and a value, returned in x0 and x1.
#[repr(C)]
pub struct Answer {
    code: u32,
    value: u64,
}

const OK: u32 = 0;
const STREAM_OUT: u32 = 1;
const WIRE_VERSION: u32 = 1;
const TABLE_SIZE: u32 = 80;
const EXCHANGE_MIN: usize = 256;

#[repr(C)]
pub struct HostTable {
    /* 0 */ version: u32,
    /* 4 */ size: u32,
    /* 8 */ host: *mut u8,
    /* 16 */ exchange: *mut u8,
    /* 24 */ exchange_len: usize,
    /* 32 */ write: extern "C" fn(*mut u8, u32, usize, usize) -> Answer,
    /* 40 */ clock: extern "C" fn(*mut u8, u32) -> Answer,
    /* 48 */ arg: extern "C" fn(*mut u8, u64) -> Answer,
    /* 56 */ grant: extern "C" fn(*mut u8, usize, usize, usize, usize) -> Answer,
    /* 64 */ effect: extern "C" fn(*mut u8, usize, usize, u64) -> Answer,
    /* 72 */ exit: extern "C-unwind" fn(*mut u8, i32) -> !,
}

// bstd's prelude, cut to what `main` uses.

/// The context every effect takes (bstd's `Cx`).
pub struct Cx {
    depth: u32,
}

/// The process's capabilities (bstd's `Io`): here, the host table.
pub struct Io {
    table: *const HostTable,
}

/// A stream (bstd's `Out`).
pub struct Out {
    table: *const HostTable,
    stream: u32,
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
        if self.table as usize == 0 { Err(Missing) } else { Ok(Out { table: self.table, stream: STREAM_OUT }) }
    }
}

impl Out {
    /// `text` and a line feed, in one host write.
    pub fn line(&self, cx: &Cx, text: &str) -> Result<(), OutError> {
        let _ = cx.depth;
        let table = unsafe { &*self.table };
        // A `&str` is its address and its length, as `core`'s `str::len` reads it.
        let (from, len): (*const u8, usize) = unsafe { transmute(text) };
        if len + 1 > table.exchange_len {
            return Err(OutError);
        }
        let to = table.exchange as usize;
        let mut i = 0;
        while i < len {
            unsafe { *((to + i) as *mut u8) = *((from as usize + i) as *const u8) };
            i += 1;
        }
        unsafe { *((to + len) as *mut u8) = b'\n' };
        let answer = (table.write)(table.host, self.stream, 0, len + 1);
        if answer.code != OK || answer.value != (len + 1) as u64 { Err(OutError) } else { Ok(()) }
    }
}

// The owner's program, unchanged.

fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, "Hello, world!")?;
    Ok(())
}

// bstd's hosted entry: checks the table as wire.rs's guest does, then runs `main`.

#[unsafe(no_mangle)]
pub extern "C" fn bstd_hosted_start(table: *const HostTable) -> i32 {
    if table as usize == 0 {
        return 101;
    }
    let t = unsafe { &*table };
    if t.version != WIRE_VERSION || t.size < TABLE_SIZE || t.exchange as usize == 0 || t.exchange_len < EXCHANGE_MIN {
        return 101;
    }
    let cx = Cx { depth: 0 };
    let io = Io { table };
    match main(&cx, io) {
        Ok(()) => 0,
        Err(error) => error.code,
    }
}
