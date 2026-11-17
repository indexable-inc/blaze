//! What makes one parameter of a `#[bstd::test]` (D109): [`Param`], ONE trait per position,
//! and where each comes from, [`From`].
//!
//! ```ignore
//! #[bstd::test(cases = 64)]
//! fn sorted_stays_sorted(cx: &Cx, xs: Vec<u32, _>, clock: Clock) { .. }
//! ```
//!
//! - A CAPABILITY (`clock`) is a [`Grant`]: made from the test's simulated world, narrowed to
//!   that one thing, through the launcher's [`grant::Checking`] and [`grant::Filled`] as `main`'s are (one
//!   mechanism). The list is [`param!`]'s below, the types `impl Grant` names except [`Io`]:
//!   a whole `Io` cannot be asked for by a test (D109).
//! - A DRAWN value (`xs`) is any type that is `blaze_sim::AnyIn<&Frame>` and `Debug`: drawn
//!   from the case's tape by blaze's own test engine, IN THE START'S FRAME (`cx.frame()`),
//!   nothing ambient (rule 75). `Vec<u32, _>` is spelled with `_` for the allocator: the
//!   attribute turns the body into a closure made fresh each start, whose `_` is inferred as
//!   that start's `&Frame` (a `_` in a `fn` signature is E0121; in a closure's it is inferred).
//!   `Vec<u32, &Frame>` written out is the same type. Behind bstd's feature `sim-draws`, which
//!   a crate turns on in its `[dev-dependencies]`: a program that writes no test never
//!   compiles the test engine, and under blaze's own build only what a test reaches is built.
//!
//! Disjoint by coherence: a capability is a type DEFINED in bstd (all of [`param!`]'s list
//! are; none is re-exported from another crate), so `Clock: AnyIn<_>` can only be written in
//! bstd, which does not write it, and blaze-sim does not depend on bstd: rustc accepts the
//! blanket impl beside the list. The trait is sealed, so no other crate adds a third source. A
//! type that is neither is refused at the test's signature by [`Param`]'s message, which names
//! both traits.

#[cfg(any(test, feature = "sim-draws"))]
use core::{cell::RefCell, fmt};
use core::marker::PhantomData;

#[cfg(any(test, feature = "sim-draws"))]
use blaze_own::String;

#[cfg(any(test, feature = "sim-draws"))]
use crate::grant::LaunchAlloc;
use crate::{
    cx::Cx,
    fs::{Dir, ReadOnly, ReadWrite},
    grant::{self, Alternatives, Checking, Form, Grant, Kind, Many, Maybe, Narrow, OneOf},
};

/// Where a parameter comes from, by its type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum From {
    /// A capability of this kind, from the test's world.
    Grant(Kind),
    /// A value drawn from the case's tape.
    Drawn,
}

/// What a failing drawn case adds to granted.rs's report: each drawn parameter's shrunk value
/// (lines `  name = <Debug>`, under a line saying how it shrank) and the tape as blaze-sim's
/// text, which the replay line gives as `BLAZE_SIM_TAPE`.
#[derive(Clone, Copy, Debug)]
pub struct DrawnReport<'a> {
    pub values: &'a str,
    pub tape: &'a str,
}

/// One parameter after `cx`, as the attribute lists it in const: its name and its source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    pub name: &'static str,
    pub from: From,
    /// A capability's form and direction (grant.rs's compose.rs); a drawn value's is plain.
    pub shape: (Form, Narrow),
}

/// The slot of the parameter `name` of type `T`: `slot::<Vec<u32, _>>("xs")` in a const
/// initializer, where `_` is inferred.
pub const fn slot<'f, T: Param<'f>>(name: &'static str) -> Slot {
    Slot { name, from: T::FROM, shape: T::SHAPE }
}

/// How many of `slots` are drawn: [`super::granted::Planned`]'s `drawn`.
pub const fn drawn(slots: &[Slot]) -> u32 {
    let (mut count, mut i) = (0, 0);
    while i < slots.len() {
        if matches!(slots[i].from, From::Drawn) {
            count += 1;
        }
        i += 1;
    }
    count
}

/// How many of `slots` are capabilities: the length of [`grants`]' array.
pub const fn grant_count(slots: &[Slot]) -> usize {
    let (mut count, mut i) = (0, 0);
    while i < slots.len() {
        if matches!(slots[i].from, From::Grant(_)) {
            count += 1;
        }
        i += 1;
    }
    count
}

/// The capabilities of `slots`, in order, as the manifest's parameters: a drawn value is no
/// grant, so the manifest (and the run spec, and `Checking::filled`'s check that every grant has
/// a parameter) never sees it. `N` is [`grant_count`]; any other `N` is a compile error.
pub const fn grants<const N: usize>(slots: &[Slot]) -> [grant::Param; N] {
    if grant_count(slots) != N {
        panic!("bstd::test: `grants::<N>` with N other than the count of capabilities");
    }
    let mut out = [grant::Param::new("_", Kind::Args); N];
    let (mut at, mut i) = (0, 0);
    while i < slots.len() {
        if let From::Grant(kind) = slots[i].from {
            out[at] = grant::Param::new(slots[i].name, kind).shaped(slots[i].shape.0, slots[i].shape.1);
            at += 1;
        }
        i += 1;
    }
    out
}

mod sealed {
    /// Only bstd says what a test's parameter may be: the blanket of drawn values and
    /// `param!`'s capabilities.
    pub trait Sealed<'f> {}
}

/// One parameter of a test after `cx`: checked with the others before any is made (a
/// capability's grant, as `main`'s), then made for one start.
#[cfg_attr(
    any(test, feature = "sim-draws"),
    diagnostic::on_unimplemented(
        message = "`{Self}` is neither a capability nor a drawn value, so a `#[bstd::test]` cannot take it",
        label = "neither `bstd::grant::Grant` (a capability) nor `blaze_sim::AnyIn<&Frame>` + `Debug` (a drawn value)",
        note = "a parameter after `cx` is a capability (`Clock`, `Dir<ReadWrite>`, `Net`, `Env`, `Out`, ...: `bstd::grant::Grant`) or a value drawn from the tape (`blaze_sim::AnyIn<&Frame>` and `Debug`: a user's type with no allocator in it gets `blaze_sim::any_in_by_any!(Type);`); a whole `Io` is neither"
    )
)]
#[cfg_attr(
    not(any(test, feature = "sim-draws")),
    diagnostic::on_unimplemented(
        message = "`{Self}` is not a capability (`bstd::grant::Grant`), and drawn values (`blaze_sim::AnyIn<&Frame>`) are off: turn on bstd's feature `sim-draws` in [dev-dependencies]",
        label = "neither `bstd::grant::Grant` nor, with `sim-draws` off, `blaze_sim::AnyIn<&Frame>`",
        note = "`[dev-dependencies] bstd = { ..., features = [\"sim-draws\"] }`: a drawn parameter needs blaze's test engine, which a build without tests never compiles"
    )
)]
pub trait Param<'f>: Sized + sealed::Sealed<'f> {
    /// Where it comes from: the attribute's const plan counts drawn values and reads kinds.
    const FROM: From;

    /// A capability's form and direction, for the test's manifest; a drawn value's is plain.
    const SHAPE: (Form, Narrow) = (Form::One, Narrow::None);

    /// What its check keeps for its making: a capability is MADE by its check, once (grant.rs's
    /// made.rs, rule 74), so it keeps itself; a drawn value keeps nothing (`()`).
    type Checked;

    /// Checked with the others before `main`'s body: a capability made from its grant (a
    /// problem is kept by `checking`); a drawn value has nothing to check.
    fn check<'c, 'g, M>(checking: Checking<'c, 'g, M>, name: &'static str) -> Checking<'c, 'g, (M, Self::Checked)>;

    /// Made for this start, once every check passed: a capability is what its check made; a
    /// drawn value is drawn now.
    fn make(at: &Making<'f>, checked: Self::Checked, name: &'static str) -> Self;
}

/// What one start's drawn parameters are made from: the start's context (the frame's lifetime
/// is the start's), and the case's draws.
pub struct Making<'f> {
    cx: &'f Cx,
    draws: &'f Draws<'f>,
}

impl<'f> Making<'f> {
    /// `cx` is the one [`grant::Filled::cx`] gives: the proof every check passed.
    pub fn new(cx: &'f Cx, draws: &'f Draws<'f>) -> Making<'f> {
        Making { cx, draws }
    }

    /// The start's context, whose frame a drawn value is made in.
    pub fn cx(&self) -> &'f Cx {
        self.cx
    }
}

/// What a start draws its drawn parameters from: the case's tape (live at the first start,
/// replayed from the first start's record at each restart, simtest/drawn.rs), and, at the
/// report's run, where each value's `Debug` form is shown. A test with no drawn parameter is
/// started with [`Draws::NONE`].
pub struct Draws<'t> {
    #[cfg(any(test, feature = "sim-draws"))]
    tape: Option<&'t blaze_sim::Tape>,
    #[cfg(any(test, feature = "sim-draws"))]
    shown: Option<&'t RefCell<String<LaunchAlloc>>>,
    _tape: PhantomData<&'t ()>,
}

impl Draws<'static> {
    /// No tape: a test whose parameters are all capabilities.
    pub const NONE: Draws<'static> = Draws {
        #[cfg(any(test, feature = "sim-draws"))]
        tape: None,
        #[cfg(any(test, feature = "sim-draws"))]
        shown: None,
        _tape: PhantomData,
    };
}

#[cfg(any(test, feature = "sim-draws"))]
impl<'t> Draws<'t> {
    /// Draws from `tape`; with `shown`, each value's `Debug` form is written there as made.
    pub fn of(tape: &'t blaze_sim::Tape, shown: Option<&'t RefCell<String<LaunchAlloc>>>) -> Draws<'t> {
        Draws { tape: Some(tape), shown, _tape: PhantomData }
    }
}

/// The most characters of one drawn value's `Debug` form a report shows.
#[cfg(any(test, feature = "sim-draws"))]
pub const SHOWN_MOST: usize = 240;

/// `  name = <Debug, at most SHOWN_MOST characters>\n` into `out`.
#[cfg(any(test, feature = "sim-draws"))]
fn show(out: &mut String<LaunchAlloc>, name: &str, value: &dyn fmt::Debug) {
    use fmt::Write as _;
    let _ = out.write_str("  ");
    let _ = out.write_str(name);
    let _ = out.write_str(" = ");
    let mut capped = Capped { out, left: SHOWN_MOST, cut: false };
    // An `Err` is the cap reached: `capped.cut` says so below.
    let _ = write!(capped, "{value:?}");
    let cut = capped.cut;
    if cut {
        let _ = out.write_str(" ...(cut at ");
        let _ = write!(out, "{SHOWN_MOST}");
        let _ = out.write_str(" characters)");
    }
    let _ = out.write_str("\n");
}

/// A writer that takes at most `left` characters, then refuses.
#[cfg(any(test, feature = "sim-draws"))]
struct Capped<'o> {
    out: &'o mut String<LaunchAlloc>,
    left: usize,
    cut: bool,
}

#[cfg(any(test, feature = "sim-draws"))]
impl fmt::Write for Capped<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for c in text.chars() {
            if self.left == 0 {
                self.cut = true;
                return Err(fmt::Error);
            }
            self.out.write_char(c)?;
            self.left -= 1;
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "sim-draws"))]
impl<'f, T: blaze_sim::AnyIn<&'f crate::mem::Frame> + fmt::Debug> sealed::Sealed<'f> for T {}

/// Every drawn value: from the case's tape, in the start's frame.
#[cfg(any(test, feature = "sim-draws"))]
impl<'f, T: blaze_sim::AnyIn<&'f crate::mem::Frame> + fmt::Debug> Param<'f> for T {
    const FROM: From = From::Drawn;

    type Checked = ();

    fn check<'c, 'g, M>(checking: Checking<'c, 'g, M>, _name: &'static str) -> Checking<'c, 'g, (M, ())> {
        checking.beside(())
    }

    fn make(at: &Making<'f>, _checked: (), name: &'static str) -> T {
        let Some(tape) = at.draws.tape else {
            // Planned's `drawn` sends every test with a drawn parameter to simtest/drawn.rs,
            // which always hands a tape: this is that routing broken, named.
            panic!("bstd::test: the parameter `{name}` is drawn and this start has no tape: Planned.drawn did not route the test to simtest::drawn")
        };
        let value = T::any_in(tape, name, at.cx().frame());
        if let Some(shown) = at.draws.shown {
            show(&mut shown.borrow_mut(), name, &value);
        }
        value
    }
}

/// `Param` for each capability a test may take: the types `impl Grant` names, less `Io`
/// (keep beside grant.rs's list: a capability missing here is refused as "neither").
macro_rules! param {
    ($($ty:ty),* $(,)?) => {$(
        impl<'f> sealed::Sealed<'f> for $ty {}

        impl<'f> Param<'f> for $ty {
            const FROM: From = From::Grant(<$ty as Grant>::KIND);

            const SHAPE: (Form, Narrow) = (<$ty as Grant>::FORM, <$ty as Grant>::NARROW);

            type Checked = $ty;

            fn check<'c, 'g, M>(checking: Checking<'c, 'g, M>, name: &'static str) -> Checking<'c, 'g, (M, $ty)> {
                checking.check::<$ty>(name)
            }

            fn make(_at: &Making<'f>, checked: $ty, _name: &'static str) -> $ty {
                checked
            }
        }
    )*};
}

param! {
    crate::out::Out,
    crate::args::Args,
    crate::clock::Clock,
    crate::mem::Mem,
    crate::mem::Heap,
    grant::Spec,
    Dir<ReadOnly>,
    Dir<ReadWrite>,
    crate::net::Net,
    crate::env::Env,
    crate::input::In,
    crate::fs::Fs,
    crate::rng::Rng,
    crate::host::Host,
    crate::streams::Streams,
    crate::term::Term,
    crate::keys::Keys,
    crate::roots::Roots,
    crate::locale::Locale,
    crate::user::User,
    crate::code::Code,
    crate::window::Window,
    crate::automation::Automation,
    crate::opener::Opener,
    crate::power::Power,
    crate::audio::Audio,
    crate::device::Camera,
    crate::device::Screen,
    crate::device::Location,
    crate::device::Bluetooth,
    crate::device::Usb,
    crate::device::Serial,
    crate::device::Clipboard,
    crate::device::Notify,
    crate::audio::AudioOut,
    crate::audio::AudioIn,
    crate::gpu::Gpu,
    grant::Connect,
    grant::Listen,
}

/// The composed forms (grant.rs's compose.rs), each a bstd type, so each generic impl is
/// disjoint from the drawn blanket as the listed capabilities are (`Maybe<G>: AnyIn<_>` names a
/// local type, and only bstd could write it). `Option<G>` is no form for this reason: it would
/// overlap a drawn `Option<u32>` (E0119).
macro_rules! composed {
    ($($(#[$doc:meta])* [$($g:tt)*] $ty:ty;)*) => {$(
        impl<'f, $($g)*> sealed::Sealed<'f> for $ty {}

        $(#[$doc])*
        impl<'f, $($g)*> Param<'f> for $ty {
            const FROM: From = From::Grant(<$ty as Grant>::KIND);

            const SHAPE: (Form, Narrow) = (<$ty as Grant>::FORM, <$ty as Grant>::NARROW);

            type Checked = $ty;

            fn check<'c, 'g, M>(checking: Checking<'c, 'g, M>, name: &'static str) -> Checking<'c, 'g, (M, $ty)> {
                checking.check::<$ty>(name)
            }

            fn make(_at: &Making<'f>, checked: $ty, _name: &'static str) -> $ty {
                checked
            }
        }
    )*};
}

composed! {
    [G: Grant] Maybe<G>;
    [T: Alternatives] OneOf<T>;
    [G: Grant] Many<G>;
}

#[cfg(feature = "requests")]
param! {
    crate::requests::Http,
}

/// The body a test's attribute hands its runner: every parameter checked, then made, then the
/// test called. The test is a closure made fresh at each call (each start), so a `_` in a
/// parameter's type is inferred as that start's (`Vec<u32, _>`: the start's `&Frame`).
/// `$cx` is the test's first parameter (`cx: &Cx`).
#[doc(hidden)]
#[macro_export]
macro_rules! __test_body {
    ($cx:tt : $cxty:ty $(, $p:tt : $ty:ty)* ; $(-> $ret:ty)? $body:block) => {
        move |given: &mut $crate::grant::Given<'_>, draws: &$crate::test::Draws<'_>| {
            let checking = given.checking();
            $( let checking = <$ty as $crate::test::Param<'_>>::check(checking, ::core::stringify!($p)); )*
            let filled = checking.filled()?;
            let cx = filled.cx();
            let making = $crate::test::Making::new(cx, draws);
            let $crate::__made!($($p),*) = filled.hand_out();
            let test = |$cx: $cxty $(, $p: $ty)*| $(-> $ret)? { $body };
            ::core::result::Result::Ok(test(
                cx
                $(, <$ty as $crate::test::Param<'_>>::make(&making, $p, ::core::stringify!($p)))*
            ))
        }
    };
}
