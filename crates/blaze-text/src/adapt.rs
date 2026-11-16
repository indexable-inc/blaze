//! The one module that names `core::fmt`, both ways across the border:
//!
//! - [`Shown`] makes any [`Show`] a `Display`, for code that still formats with `{}`.
//! - [`PutDisplay::put_display`] writes a foreign `Display` (one from outside blaze) into a
//!   [`Put`] through `fmt::Arguments`, named so its cost shows at the call.

use core::fmt;

use crate::parse::ParseError;
use crate::put::{Count, Put};
use crate::show::Show;

/// A [`Show`] as a `Display`: `format!("{}", Shown(&x))`.
#[derive(Clone, Copy)]
pub struct Shown<T>(pub T);

/// A `fmt::Formatter` as a [`Put`], keeping the first error it gave.
struct ToFormatter<'f, 'g> {
    f: &'f mut fmt::Formatter<'g>,
    result: fmt::Result,
}

impl Put for ToFormatter<'_, '_> {
    #[inline]
    fn put_str(&mut self, text: &str) {
        if self.result.is_ok() {
            self.result = self.f.write_str(text);
        }
    }
}

impl<T: Show> fmt::Display for Shown<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Width and alignment from the format string apply, measured in characters.
        let mut out = ToFormatter { f, result: Ok(()) };
        if let Some(width) = out.f.width() {
            let mut count = Count::new();
            self.0.show(&mut count);
            let pad = width.saturating_sub(count.chars);
            let fill = out.f.fill();
            let (before, after) = match out.f.align() {
                Some(fmt::Alignment::Right) => (pad, 0),
                Some(fmt::Alignment::Center) => (pad / 2, pad - pad / 2),
                Some(fmt::Alignment::Left) | None => (0, pad),
            };
            let mut one = [0u8; 4];
            let fill = fill.encode_utf8(&mut one);
            for _ in 0..before {
                out.put_str(fill);
            }
            self.0.show(&mut out);
            for _ in 0..after {
                out.put_str(fill);
            }
            return out.result;
        }
        self.0.show(&mut out);
        out.result
    }
}

/// What it wraps, so a value handed out as `Shown` for `{}` users is still a [`Show`] to a
/// chain.
impl<T: Show> Show for Shown<T> {
    #[inline]
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        self.0.show(out);
    }
}

impl<T: Show> fmt::Debug for Shown<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A [`Put`] as a `fmt::Write`.
struct FromFormat<'p, P: Put + ?Sized>(&'p mut P);

impl<P: Put + ?Sized> fmt::Write for FromFormat<'_, P> {
    #[inline]
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.put_str(text);
        Ok(())
    }
}

/// Writing a foreign `Display` through `core::fmt`.
pub trait PutDisplay: Put {
    /// `value`'s `Display` form, through `fmt::Arguments` (the slow path this crate exists to
    /// avoid: for values whose only text is a `Display`, such as another crate's error).
    ///
    /// # Panics
    ///
    /// When `value`'s `Display` returns an error while the sink did not: a `Display` impl
    /// that fails on its own is a bug in it.
    fn put_display(&mut self, value: &dyn fmt::Display) -> &mut Self {
        let mut out = FromFormat(self);
        if fmt::write(&mut out, format_args!("{value}")).is_err() {
            panic!("blaze_text::put_display: a `Display` impl returned an error on its own");
        }
        out.0
    }
}

impl<P: Put + ?Sized> PutDisplay for P {}

/// `format_args!(..)` as a [`Show`]: what keeps a `format_args!` caller of a writer that takes
/// a `Show` (bstd's `Out::line`) compiling until it is a chain. Through `core::fmt`, so it
/// costs what `{}` costs; text with no arguments goes straight to the sink.
///
/// # Panics
///
/// When a `Display` inside returns an error on its own (the sink here cannot fail): the
/// contract `alloc::string::ToString` also holds a `Display` to.
impl Show for fmt::Arguments<'_> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        if let Some(text) = self.as_str() {
            out.put_str(text);
            return;
        }
        if fmt::write(&mut FromFormat(out), *self).is_err() {
            panic!("blaze_text: a `Display` inside `format_args!` returned an error on its own");
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Shown(self), f)
    }
}

impl core::error::Error for ParseError {}
