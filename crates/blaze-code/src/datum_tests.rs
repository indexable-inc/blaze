//! [`crate::datum`]: a static's datum seals into one form, reads back in place, refuses what
//! breaks the form, and fills at its address against the run's words.

use blaze_own::{AVec, Alloc, Vec};

use crate::datum::{Datum, DatumError, DatumView, FillError, OWN_DATUM, Place, fill};
use crate::image::ImageError;
use crate::place::Words;
use crate::record::{Space, Symbol, Target, Word};

const F_THUNK: u64 = 0x2_0000_0040;
const S_AT: u64 = 0x3_0000_1000;

fn func(name: &str) -> Target {
    Target { space: Space::Func, symbol: Symbol::of_name(name) }
}

fn static_(name: &str) -> Target {
    Target { space: Space::Static, symbol: Symbol::of_name(name) }
}

/// The run's words: `f`'s entry thunk and static `s`'s address; anything else is unbound.
struct Run;

impl Words for Run {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        match target {
            t if t == func("f") => Ok(F_THUNK),
            t if t == static_("s") => Ok(S_AT),
            _ => Err(ImageError::Unbound { target }),
        }
    }
}

fn encode(d: Datum<'static>) -> AVec<'static, u8> {
    let sealed = d.seal().expect("the datum seals");
    let mut out = Vec::new_in(Alloc::global());
    sealed.encode(&mut out);
    out
}

/// A read-only datum of plain bytes reads back as written and fills to its bytes; one with
/// words fills each from the run's words or its own address, in any order the builder added
/// targets (two builders write equal bytes). Prints "datum: plain and worded data round-trip
/// and fill".
#[test]
fn a_datum_round_trips_and_fills() {
    let alloc = Alloc::global();
    let mut plain = Datum::new_in(Place::ReadOnly, 0, alloc);
    plain.bytes.extend_from_slice(b"Hello, world!\n");
    let bytes = encode(plain);
    let view = DatumView::read(&bytes).expect("the plain datum reads");
    assert_eq!(view.place(), Place::ReadOnly);
    assert_eq!((view.len(), view.align(), view.words(), view.targets()), (14, 1, 0, 0));
    let mut out = [0u8; 14];
    fill(&view, Symbol::of_name("hello"), 0x1001, &mut Run, &mut out).expect("a plain datum fills at any byte");
    assert_eq!(&out, b"Hello, world!\n");

    // `static P: (&u8, fn(), &u8) = (&S, f, &OWN[1])` with its own allocation after its words.
    let worded = |order: [Target; 2]| {
        let mut d = Datum::new_in(Place::ReadOnly, 3, alloc);
        d.bytes.extend_from_slice(&[0; 24]);
        d.bytes.extend_from_slice(&[7, 8]);
        let a = d.target(order[0]);
        let b = d.target(order[1]);
        let (s, f) = if order[0] == static_("s") { (a, b) } else { (b, a) };
        d.words.push(Word { at: 8, target: f, addend: 0 });
        d.words.push(Word { at: 0, target: s, addend: 0 });
        d.words.push(Word { at: 16, target: OWN_DATUM, addend: 25 });
        encode(d)
    };
    let one = worded([static_("s"), func("f")]);
    let two = worded([func("f"), static_("s")]);
    assert_eq!(one, two, "the builder's order of targets leaves no trace");
    let view = DatumView::read(&one).expect("the worded datum reads");
    assert_eq!((view.words(), view.targets()), (3, 2));
    let at = 0x5_0000_0008;
    let mut out = [0u8; 26];
    fill(&view, Symbol::of_name("p"), at, &mut Run, &mut out).expect("the worded datum fills");
    let word = |i: usize| u64::from_le_bytes(out[i..i + 8].try_into().expect("8 bytes"));
    assert_eq!((word(0), word(8), word(16)), (S_AT, F_THUNK, at + 25));
    assert_eq!(&out[24..], &[7, 8], "the bytes past the words are copied");
    println!("datum: plain and worded data round-trip and fill");
}

/// Each rule refuses by name, and a control that keeps the rule seals: a word past the bytes,
/// a word in a datum aligned below 8, two words at one offset, an own word past the end, a
/// thread-local target, a zeroed datum with bytes, an alignment past a page; a fill at a
/// misaligned address and one naming an unbound static. Prints "datum: 9 refusals, each with
/// its control".
#[test]
fn a_datum_that_breaks_the_form_is_refused_by_name() {
    let alloc = Alloc::global();
    let base = || {
        let mut d = Datum::new_in(Place::Writable, 3, alloc);
        d.bytes.extend_from_slice(&[0; 16]);
        d
    };
    let seal = |d: Datum<'static>| d.seal().map(|_| ());
    assert_eq!(seal(base()), Ok(()), "the control seals");

    let mut d = base();
    d.words.push(Word { at: 16, target: OWN_DATUM, addend: 0 });
    assert_eq!(seal(d), Err(DatumError::WordOutside { at: 16, len: 16 }));

    let mut d = base();
    d.align = 2;
    d.words.push(Word { at: 8, target: OWN_DATUM, addend: 0 });
    assert_eq!(seal(d), Err(DatumError::WordMisaligned { at: 8 }));

    let mut d = base();
    d.words.push(Word { at: 8, target: OWN_DATUM, addend: 0 });
    d.words.push(Word { at: 8, target: OWN_DATUM, addend: 4 });
    assert_eq!(seal(d), Err(DatumError::WordsOverlap { at: 8 }));

    let mut d = base();
    d.words.push(Word { at: 0, target: OWN_DATUM, addend: 17 });
    assert_eq!(seal(d), Err(DatumError::OwnOutside { at: 0, addend: 17, len: 16 }));

    let mut d = base();
    let tls = Target { space: Space::Tls, symbol: Symbol::of_name("t") };
    d.target(tls);
    assert_eq!(seal(d), Err(DatumError::Space { space: Space::Tls, symbol: tls.symbol }));

    let mut d = Datum::new_in(Place::Zeroed { len: 64 }, 3, alloc);
    assert_eq!(seal(Datum::new_in(Place::Zeroed { len: 64 }, 3, alloc)), Ok(()), "a zeroed control seals");
    d.bytes.push(1);
    assert_eq!(seal(d), Err(DatumError::ZeroedHolds));

    let mut d = base();
    d.align = 13;
    assert_eq!(seal(d), Err(DatumError::Align { align: 13 }));

    // The fill's two refusals over a datum naming static `s` and an unbound one.
    let mut d = base();
    let s = d.target(static_("s"));
    d.words.push(Word { at: 0, target: s, addend: 0 });
    let bytes = encode(d);
    let view = DatumView::read(&bytes).expect("reads");
    let mut out = [0u8; 16];
    let sym = Symbol::of_name("q");
    assert_eq!(fill(&view, sym, 0x1004, &mut Run, &mut out), Err(FillError::Misaligned { symbol: sym, at: 0x1004, align: 8 }));
    assert_eq!(fill(&view, sym, 0x1008, &mut Run, &mut out), Ok(()), "the aligned control fills");
    let mut d = base();
    let gone_at = d.target(static_("gone"));
    d.words.push(Word { at: 8, target: gone_at, addend: 0 });
    let bytes = encode(d);
    let view = DatumView::read(&bytes).expect("reads");
    assert_eq!(
        fill(&view, sym, 0x1008, &mut Run, &mut out),
        Err(FillError::Word { symbol: sym, why: ImageError::Unbound { target: static_("gone") } })
    );
    println!("datum: 9 refusals, each with its control");
}

/// Bytes from a store are checked once on read: a cut datum, an unknown place, a target of a
/// refused space and targets out of order are refused, never misread. Prints "datum: every cut
/// refused; a forged space and order refused".
#[test]
fn bytes_that_are_no_datum_do_not_read() {
    let alloc = Alloc::global();
    let mut d = Datum::new_in(Place::ReadOnly, 3, alloc);
    d.bytes.extend_from_slice(&[0; 16]);
    let a = d.target(func("f"));
    let b = d.target(static_("s"));
    d.words.push(Word { at: 0, target: a, addend: 0 });
    d.words.push(Word { at: 8, target: b, addend: 0 });
    let bytes = encode(d);
    assert!(DatumView::read(&bytes).is_ok(), "the control reads");
    for cut in 0..bytes.len() {
        assert!(DatumView::read(&bytes[..cut]).is_err(), "a datum cut to {cut} bytes was read");
    }
    let mut forged = bytes.clone();
    forged[8] = 9;
    assert_eq!(DatumView::read(&forged).err(), Some(DatumError::PlaceByte { byte: 9 }));
    // The targets start after the head (40) and the 16 bytes.
    let first = 40 + 16;
    let mut forged = bytes.clone();
    forged[first] = Space::Tls as u8;
    assert!(matches!(DatumView::read(&forged), Err(DatumError::Space { space: Space::Tls, .. })));
    let mut swapped = bytes.clone();
    let (one, two) = swapped[first..first + 80].split_at_mut(40);
    one.swap_with_slice(two);
    assert_eq!(DatumView::read(&swapped).err(), Some(DatumError::TargetsOrder));
    println!("datum: every cut refused; a forged space and order refused");
}

/// The address the C name test's run binds `puts` to.
const PUTS_AT: u64 = 0x7_0000_0010;

/// The run's words over a datum naming C names: a C name is bound by the text the datum keeps
/// (`DatumView::name`), as the launcher's binder does; `puts` is bound, any other is not.
struct ByName<'v>(DatumView<'v>);

impl Words for ByName<'_> {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        match (target.space, self.0.name(target.symbol)) {
            (Space::Outside, Some("puts")) => Ok(PUTS_AT),
            (Space::Func, _) if target == func("f") => Ok(F_THUNK),
            _ => Err(ImageError::Unbound { target }),
        }
    }
}

fn sealed_bytes(d: Datum<'static>) -> AVec<'static, u8> {
    let sealed = d.seal().expect("the datum seals");
    let mut out = Vec::new_in(blaze_own::testing::heap().alloc());
    sealed.encode(&mut out);
    out
}

/// A datum names a C name (a C table of function pointers: `{ f, puts, puts + 4 }`): it seals
/// with the name's text whatever order its targets came in, reads back with `name` giving the
/// text for the symbol, and fills each C name word from the run's binding of that text. A datum
/// naming no C name keeps the bytes of the form before C names (the fourth count 0). Refused,
/// each with its control: a C name added as a bare target (no text), a forged text that does
/// not hash to its symbol, a names section where no C name is named, and nonzero padding.
/// Prints "datum: C names seal, read, fill; 4 refusals".
#[test]
fn a_datum_names_a_c_name_by_its_text() {
    let alloc = blaze_own::testing::heap().alloc();
    let table = |puts_first: bool| {
        let mut d = Datum::new_in(Place::ReadOnly, 3, alloc);
        d.bytes.extend_from_slice(&[0; 24]);
        let (p, f) = if puts_first {
            let p = d.outside("puts");
            (p, d.target(func("f")))
        } else {
            let f = d.target(func("f"));
            (d.outside("puts"), f)
        };
        // The same name added twice is one target and one text.
        let again = d.outside("puts");
        d.words.push(Word { at: 0, target: f, addend: 0 });
        d.words.push(Word { at: 8, target: p, addend: 0 });
        d.words.push(Word { at: 16, target: again, addend: 4 });
        sealed_bytes(d)
    };
    let one = table(true);
    assert_eq!(one, table(false), "the builder's order of targets and names leaves no trace");
    let view = DatumView::read(&one).expect("a datum naming a C name reads");
    assert_eq!(view.targets(), 2, "`puts` twice is one target");
    assert_eq!(view.name(Symbol::of_name("puts")), Some("puts"));
    assert_eq!(view.name(Symbol::of_name("printf")), None, "a name the datum does not name has no text");
    let mut out = [0u8; 24];
    fill(&view, Symbol::of_name("table"), 0x4000, &mut ByName(view), &mut out).expect("the C name words fill");
    let word = |i: usize| u64::from_le_bytes(out[i..i + 8].try_into().expect("8 bytes"));
    assert_eq!((word(0), word(8), word(16)), (F_THUNK, PUTS_AT, PUTS_AT + 4));

    // The control: no C name, so no names section and the fourth count 0.
    let mut plain = Datum::new_in(Place::ReadOnly, 3, alloc);
    plain.bytes.extend_from_slice(&[0; 8]);
    let f = plain.target(func("f"));
    plain.words.push(Word { at: 0, target: f, addend: 0 });
    let plain = sealed_bytes(plain);
    assert_eq!(&plain[36..40], &[0; 4], "a datum naming no C name keeps the old form's fourth count");
    assert_eq!(plain.len(), 40 + 8 + 40 + 16, "and no names section");

    // A C name as a bare target has no text: refused at the seal.
    let mut bare = Datum::new_in(Place::ReadOnly, 3, alloc);
    bare.bytes.extend_from_slice(&[0; 8]);
    let puts = Target { space: Space::Outside, symbol: Symbol::of_name("puts") };
    let at = bare.target(puts);
    bare.words.push(Word { at: 0, target: at, addend: 0 });
    assert_eq!(bare.seal().err(), Some(DatumError::NoName { symbol: puts.symbol }));

    // The names section is after the head, 24 bytes, 2 targets and 3 words: one row, then "puts".
    let section = 40 + 24 + 2 * 40 + 3 * 16;
    let mut forged = one.clone();
    forged[section + 8] = b'q';
    assert_eq!(DatumView::read(&forged).err(), Some(DatumError::NameWrong { symbol: puts.symbol }));
    let mut forged = one.clone();
    forged[section + 12] = 1;
    assert!(matches!(DatumView::read(&forged), Err(DatumError::Names { .. })), "nonzero padding is refused");
    let mut forged = plain.clone();
    forged[36] = 8;
    forged.extend_from_slice(&[0; 8]);
    assert!(matches!(DatumView::read(&forged), Err(DatumError::Names { .. })), "a section with no C name is refused");
    println!("datum: C names seal, read, fill; 4 refusals");
}
