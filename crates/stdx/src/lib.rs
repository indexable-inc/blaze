//! Missing batteries for standard libraries.

use std::borrow::Cow;
use std::io as sio;
use std::process::Command;
use std::{cmp::Ordering, ops, time::Instant};

mod macros;

pub mod assert;
pub mod edit_distance;
pub mod lend;
pub mod list;
pub mod panic_context;
pub mod tempfile;
pub mod time;

pub use itertools;
pub use lend::lend;

#[inline(always)]
pub const fn is_ci() -> bool {
    option_env!("CI").is_some()
}

/// Runs `f` with at least 100 KiB of stack left, on a new 1 MiB segment if this one has less,
/// as rustc's `ensure_sufficient_stack` does: that of `rustc_type_ir` does nothing outside
/// rustc. What recurses once per level of what it walks, the trait solver or the lowering of
/// an expression, goes as deep as the source does, and an unoptimized build overflows a 2 MB
/// thread long before rustc's 5,204-arm `else if` is through. Unoptimized, a query's frames
/// between two checks (a fetch through blaze-query into the solver) take more than 100 KiB,
/// so a debug build keeps 1 MiB left and grows by 8 MiB.
///
/// On a fiber, stacker's own limit is wrong: it keeps one per thread, from the thread's stack,
/// so the fiber's limit is kept here instead ([`set_fiber_stack_limit`], set by the fiber
/// driver at each job start and carried across a switch).
#[cfg(not(target_family = "wasm"))]
pub fn ensure_sufficient_stack<R>(f: impl FnOnce() -> R) -> R {
    const RED_ZONE: usize = if cfg!(debug_assertions) { 1024 * 1024 } else { 100 * 1024 };
    const SEGMENT: usize = if cfg!(debug_assertions) { 8 * 1024 * 1024 } else { 1024 * 1024 };
    let limit = fiber_stack_limit();
    if limit == 0 {
        return stacker::maybe_grow(RED_ZONE, SEGMENT, f);
    }
    if stack_pointer().saturating_sub(limit) >= RED_ZONE {
        return f();
    }
    stacker::grow(SEGMENT, || {
        // Inside `grow` stacker's limit is the new segment's, so its remaining stack is right.
        let remaining = stacker::remaining_stack().expect("stacker knows the segment it made");
        let _outer = FiberLimit::set(stack_pointer() - remaining);
        f()
    })
}

#[cfg(not(target_family = "wasm"))]
thread_local! {
    static FIBER_STACK_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The lowest usable address of the fiber stack this thread runs now; 0 on the thread's own
/// stack. `inline(never)`: a job can resume on another thread after a wait, and an inlined
/// thread-local's address could be kept across it.
#[cfg(not(target_family = "wasm"))]
#[inline(never)]
pub fn fiber_stack_limit() -> usize {
    FIBER_STACK_LIMIT.get()
}

/// Sets [`fiber_stack_limit`]; `inline(never)` for the same reason.
#[cfg(not(target_family = "wasm"))]
#[inline(never)]
pub fn set_fiber_stack_limit(limit: usize) {
    FIBER_STACK_LIMIT.set(limit);
}

/// The fiber limit for a grown segment, put back to the outer one when the segment's call
/// ends, by return or by unwind.
#[cfg(not(target_family = "wasm"))]
struct FiberLimit(usize);

#[cfg(not(target_family = "wasm"))]
impl FiberLimit {
    fn set(limit: usize) -> FiberLimit {
        let outer = fiber_stack_limit();
        set_fiber_stack_limit(limit);
        FiberLimit(outer)
    }
}

#[cfg(not(target_family = "wasm"))]
impl Drop for FiberLimit {
    fn drop(&mut self) {
        set_fiber_stack_limit(self.0);
    }
}

#[cfg(not(target_family = "wasm"))]
#[inline(always)]
fn stack_pointer() -> usize {
    let here = 0u8;
    std::ptr::addr_of!(here).addr()
}

#[cfg(target_family = "wasm")]
pub fn ensure_sufficient_stack<R>(f: impl FnOnce() -> R) -> R {
    f()
}

pub fn hash_once<Hasher: std::hash::Hasher + Default>(thing: impl std::hash::Hash) -> u64 {
    std::hash::BuildHasher::hash_one(&std::hash::BuildHasherDefault::<Hasher>::default(), thing)
}

/// The text a panic was raised with, as `catch_unwind` or a joined thread hands its payload:
/// `panic!`'s `&'static str` or formatted `String`. `None` for any other payload.
// dyn: a panic's payload, read once per panic caught
pub fn panic_text(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    match payload.downcast_ref::<&'static str>() {
        Some(text) => Some(text),
        None => payload.downcast_ref::<String>().map(String::as_str),
    }
}

#[must_use]
#[expect(clippy::print_stderr, reason = "only visible to developers")]
pub fn timeit(label: &'static str) -> impl Drop {
    let start = Instant::now();
    defer(move || eprintln!("{}: {:.2}", label, start.elapsed().as_nanos()))
}

pub trait TupleExt {
    type Head;
    type Tail;
    fn head(self) -> Self::Head;
    fn tail(self) -> Self::Tail;
}

impl<T, U> TupleExt for (T, U) {
    type Head = T;
    type Tail = U;
    fn head(self) -> Self::Head {
        self.0
    }
    fn tail(self) -> Self::Tail {
        self.1
    }
}

impl<T, U, V> TupleExt for (T, U, V) {
    type Head = T;
    type Tail = V;
    fn head(self) -> Self::Head {
        self.0
    }
    fn tail(self) -> Self::Tail {
        self.2
    }
}

impl<T> TupleExt for &T
where
    T: TupleExt + Copy,
{
    type Head = T::Head;
    type Tail = T::Tail;
    fn head(self) -> Self::Head {
        (*self).head()
    }
    fn tail(self) -> Self::Tail {
        (*self).tail()
    }
}

pub fn to_lower_snake_case(s: &str) -> String {
    to_snake_case(s, char::to_lowercase)
}
pub fn to_upper_snake_case(s: &str) -> String {
    to_snake_case(s, char::to_uppercase)
}

// Code partially taken from rust/compiler/rustc_lint/src/nonstandard_style.rs
// commit: 9626f2b
fn to_snake_case<F, I>(mut s: &str, change_case: F) -> String
where
    F: Fn(char) -> I,
    I: Iterator<Item = char>,
{
    let mut words = vec![];

    // Preserve leading underscores
    s = s.trim_start_matches(|c: char| {
        if c == '_' {
            words.push(String::new());
            true
        } else {
            false
        }
    });

    for s in s.split('_') {
        let mut last_upper = false;
        let mut buf = String::new();

        if s.is_empty() {
            continue;
        }

        for ch in s.chars() {
            if !buf.is_empty() && buf != "'" && ch.is_uppercase() && !last_upper {
                words.push(buf);
                buf = String::new();
            }

            last_upper = ch.is_uppercase();
            buf.extend(change_case(ch));
        }

        words.push(buf);
    }

    words.join("_")
}

// Taken from rustc.
#[must_use]
pub fn to_camel_case(ident: &str) -> String {
    ident
        .trim_matches('_')
        .split('_')
        .filter(|component| !component.is_empty())
        .map(|component| {
            let mut camel_cased_component = String::with_capacity(component.len());

            let mut new_word = true;
            let mut prev_is_lower_case = true;

            for c in component.chars() {
                // Preserve the case if an uppercase letter follows a lowercase letter, so that
                // `camelCase` is converted to `CamelCase`.
                if prev_is_lower_case && c.is_uppercase() {
                    new_word = true;
                }

                if new_word {
                    camel_cased_component.extend(c.to_uppercase());
                } else {
                    camel_cased_component.extend(c.to_lowercase());
                }

                prev_is_lower_case = c.is_lowercase();
                new_word = false;
            }

            camel_cased_component
        })
        .fold((String::new(), None), |(mut acc, prev): (_, Option<String>), next| {
            // separate two components with an underscore if their boundary cannot
            // be distinguished using an uppercase/lowercase case distinction
            let join = prev
                .and_then(|prev| {
                    let f = next.chars().next()?;
                    let l = prev.chars().last()?;
                    Some(!char_has_case(l) && !char_has_case(f))
                })
                .unwrap_or(false);
            acc.push_str(if join { "_" } else { "" });
            acc.push_str(&next);
            (acc, Some(next))
        })
        .0
}

// Taken from rustc.
#[must_use]
pub const fn char_has_case(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase()
}

#[must_use]
pub fn is_upper_snake_case(s: &str) -> bool {
    s.chars().all(|c| c.is_uppercase() || c == '_' || c.is_numeric())
}

pub fn replace(buf: &mut String, from: char, to: &str) {
    let replace_count = buf.chars().filter(|&ch| ch == from).count();
    if replace_count == 0 {
        return;
    }
    let from_len = from.len_utf8();
    let additional = to.len().saturating_sub(from_len);
    buf.reserve(additional * replace_count);

    let mut end = buf.len();
    while let Some(i) = buf[..end].rfind(from) {
        buf.replace_range(i..i + from_len, to);
        end = i;
    }
}

#[must_use]
pub fn trim_indent(mut text: &str) -> String {
    if text.starts_with('\n') {
        text = &text[1..];
    }
    let indent = indent_of(text);
    text.split_inclusive('\n')
        .map(
            |line| {
                if line.len() <= indent { line.trim_start_matches(' ') } else { &line[indent..] }
            },
        )
        .collect()
}

#[must_use]
fn indent_of(text: &str) -> usize {
    text.lines()
        .filter(|it| !it.trim().is_empty())
        .map(|it| it.len() - it.trim_start().len())
        .min()
        .unwrap_or(0)
}

#[must_use]
pub fn dedent_by(spaces: usize, text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let trimmed = line.trim_start_matches(' ');
            if line.len() - trimmed.len() <= spaces { trimmed } else { &line[spaces..] }
        })
        .collect()
}

/// Indent non empty lines, including the first line
#[must_use]
pub fn indent_string(s: &str, indent_level: u8) -> String {
    if indent_level == 0 || s.is_empty() {
        return s.to_owned();
    }
    let indent_str = "    ".repeat(indent_level as usize);
    s.split_inclusive("\n")
        .map(|line| {
            if line.trim_end().is_empty() {
                Cow::Borrowed(line)
            } else {
                format!("{indent_str}{line}").into()
            }
        })
        .collect()
}

pub fn equal_range_by<T, F>(slice: &[T], mut key: F) -> ops::Range<usize>
where
    F: FnMut(&T) -> Ordering,
{
    let start = slice.partition_point(|it| key(it) == Ordering::Less);
    let len = slice[start..].partition_point(|it| key(it) == Ordering::Equal);
    start..start + len
}

#[must_use]
pub fn defer<F: FnOnce()>(f: F) -> impl Drop {
    struct D<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for D<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    D(Some(f))
}

/// A [`std::process::Child`] wrapper that will kill the child on drop.
#[cfg_attr(not(target_family = "wasm"), repr(transparent))]
#[derive(Debug)]
pub struct JodChild(pub std::process::Child);

impl ops::Deref for JodChild {
    type Target = std::process::Child;
    fn deref(&self) -> &std::process::Child {
        &self.0
    }
}

impl ops::DerefMut for JodChild {
    fn deref_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
}

impl Drop for JodChild {
    fn drop(&mut self) {
        _ = self.0.kill();
        _ = self.0.wait();
    }
}

impl JodChild {
    pub fn spawn(mut command: Command) -> sio::Result<Self> {
        command.spawn().map(Self)
    }

    #[must_use]
    #[cfg(not(target_family = "wasm"))]
    pub fn into_inner(self) -> std::process::Child {
        // SAFETY: repr transparent, except on WASM
        unsafe { std::mem::transmute::<Self, std::process::Child>(self) }
    }
}

// feature: iter_order_by
// Iterator::eq_by
pub fn iter_eq_by<I, I2, F>(this: I2, other: I, mut eq: F) -> bool
where
    I: IntoIterator,
    I2: IntoIterator,
    F: FnMut(I2::Item, I::Item) -> bool,
{
    let mut other = other.into_iter();
    let mut this = this.into_iter();

    loop {
        let x = match this.next() {
            None => return other.next().is_none(),
            Some(val) => val,
        };

        let y = match other.next() {
            None => return false,
            Some(val) => val,
        };

        if !eq(x, y) {
            return false;
        }
    }
}

/// Returns all final segments of the argument, longest first.
pub fn slice_tails<T>(this: &[T]) -> impl Iterator<Item = &[T]> {
    (0..this.len()).map(|i| &this[i..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_a_fiber_near_its_limit_the_stack_grows_and_the_limit_comes_back() {
        // A fiber with almost no stack left, as the driver would set it at a job start.
        let fake = stack_pointer() - 4096;
        set_fiber_stack_limit(fake);
        let inside = ensure_sufficient_stack(fiber_stack_limit);
        assert_ne!(inside, fake, "the call ran on the fiber's stack instead of a new segment");
        assert_ne!(inside, 0);
        assert_eq!(fiber_stack_limit(), fake);
        let unwound = std::panic::catch_unwind(|| ensure_sufficient_stack(|| panic!("in a segment")));
        assert!(unwound.is_err());
        assert_eq!(fiber_stack_limit(), fake, "an unwind left the segment's limit behind");
        set_fiber_stack_limit(0);
    }

    #[test]
    fn test_trim_indent() {
        assert_eq!(trim_indent(""), "");
        assert_eq!(
            trim_indent(
                "
            hello
            world
"
            ),
            "hello\nworld\n"
        );
        assert_eq!(
            trim_indent(
                "
            hello
            world"
            ),
            "hello\nworld"
        );
        assert_eq!(trim_indent("    hello\n    world\n"), "hello\nworld\n");
        assert_eq!(
            trim_indent(
                "
            fn main() {
                return 92;
            }
        "
            ),
            "fn main() {\n    return 92;\n}\n"
        );
    }

    #[test]
    fn test_dedent() {
        assert_eq!(dedent_by(0, ""), "");
        assert_eq!(dedent_by(1, ""), "");
        assert_eq!(dedent_by(2, ""), "");
        assert_eq!(dedent_by(0, "foo"), "foo");
        assert_eq!(dedent_by(2, "foo"), "foo");
        assert_eq!(dedent_by(2, "  foo"), "foo");
        assert_eq!(dedent_by(2, "    foo"), "  foo");
        assert_eq!(dedent_by(2, "    foo\nbar"), "  foo\nbar");
        assert_eq!(dedent_by(2, "foo\n    bar"), "foo\n  bar");
        assert_eq!(dedent_by(2, "foo\n\n    bar"), "foo\n\n  bar");
        assert_eq!(dedent_by(2, "foo\n.\n    bar"), "foo\n.\n  bar");
        assert_eq!(dedent_by(2, "foo\n .\n    bar"), "foo\n.\n  bar");
        assert_eq!(dedent_by(2, "foo\n   .\n    bar"), "foo\n .\n  bar");
    }

    #[test]
    fn test_indent_of() {
        assert_eq!(indent_of(""), 0);
        assert_eq!(indent_of(" "), 0);
        assert_eq!(indent_of(" x"), 1);
        assert_eq!(indent_of(" x\n"), 1);
        assert_eq!(indent_of(" x\ny"), 0);
        assert_eq!(indent_of(" x\n y"), 1);
        assert_eq!(indent_of(" x\n  y"), 1);
        assert_eq!(indent_of("  x\n  y"), 2);
        assert_eq!(indent_of("  x\n  y\n"), 2);
        assert_eq!(indent_of("  x\n\n  y\n"), 2);
    }

    #[test]
    fn test_replace() {
        #[track_caller]
        fn test_replace(src: &str, from: char, to: &str, expected: &str) {
            let mut s = src.to_owned();
            replace(&mut s, from, to);
            assert_eq!(s, expected, "from: {from:?}, to: {to:?}");
        }

        test_replace("", 'a', "b", "");
        test_replace("", 'a', "😀", "");
        test_replace("", '😀', "a", "");
        test_replace("a", 'a', "b", "b");
        test_replace("aa", 'a', "b", "bb");
        test_replace("ada", 'a', "b", "bdb");
        test_replace("a", 'a', "😀", "😀");
        test_replace("😀", '😀', "a", "a");
        test_replace("😀x", '😀', "a", "ax");
        test_replace("y😀x", '😀', "a", "yax");
        test_replace("a,b,c", ',', ".", "a.b.c");
        test_replace("a,b,c", ',', "..", "a..b..c");
        test_replace("a.b.c", '.', "..", "a..b..c");
        test_replace("a.b.c", '.', "..", "a..b..c");
        test_replace("a😀b😀c", '😀', ".", "a.b.c");
        test_replace("a.b.c", '.', "😀", "a😀b😀c");
        test_replace("a.b.c", '.', "😀😀", "a😀😀b😀😀c");
        test_replace(".a.b.c.", '.', "()", "()a()b()c()");
        test_replace(".a.b.c.", '.', "", "abc");
    }

    #[test]
    fn panic_text_is_the_copies_it_replaced() {
        // blaze-engine's and hir-ty-codegen's copy, which looked for a `String` first.
        fn kept(panic: &(dyn std::any::Any + Send)) -> Option<&str> {
            panic.downcast_ref::<String>().map(String::as_str).or_else(|| panic.downcast_ref::<&str>().copied())
        }
        let payloads: [Box<dyn std::any::Any + Send>; 6] = [
            Box::new("static text"),
            Box::new(String::from("formatted 7")),
            Box::new(String::new()),
            Box::new(7u32),
            Box::new(Box::new("boxed") as Box<dyn std::any::Any + Send>),
            Box::new(()),
        ];
        for payload in &payloads {
            assert_eq!(panic_text(&**payload), kept(&**payload));
        }
        let caught = std::panic::catch_unwind(|| -> u8 { panic!("caught {}", 7) }).unwrap_err();
        assert_eq!(panic_text(&*caught), Some("caught 7"));
    }
}
