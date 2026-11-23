//! A macro call's working memory in a scratch reset after the call (lane N5,
//! [`crate::task::Par::call_scratch`]): the crate's frame holds what the names keep, and one
//! call's scratch at a time; the bytes written are the same.
//!
//! The fixture is one crate written here of [`CALLS`] calls of a `macro_rules!` that writes a
//! struct and two impls (the shape of the calls that fill core's frame: the rendering, the
//! matcher's expansion, the origin column, each grown per call). It is resolved twice, each in a
//! fresh frame: under [`Scratch::Kept`] (the CONTROL: the Par runs no work, the call's work is
//! done in the crate's frame, as before the hook) and under [`Scratch::Reset`].
//!
//! The floor (rule 67): the crate frame's peak under `Reset` is what the names keep (the texts,
//! tokens, origins, lowering and tables), with no call's work in it; the scratch's peak is one
//! call's high-water. The ratio printed is `(reset peak + scratch peak) / kept peak`.

use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use blaze_id::ItemKey;
use blaze_mem::{Frame, Tag};
use blaze_own::{AString, AVec, Alloc, Erased};

use super::entry::input;
use crate::graph::Sysroot;
use crate::resolve::{SourceTexts, resolve_crates};
use crate::task::{Par, Scratch};

/// How many calls the fixture writes.
const CALLS: usize = 2000;

/// The fixture's root file.
const ROOT: &str = "/fixture/lib.rs";

/// The one crate's one file, made in the test's frame.
struct Fixture<'t>(&'t str);

impl<'a, 't: 'a> SourceTexts<'a> for Fixture<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == ROOT).then_some(self.0)
    }
}

/// A serial Par whose call scratch is a frame of its own, reset (rolled back) after each call;
/// or, as the control, none ([`Scratch::Kept`]).
struct ScratchPar {
    mode: Scratch,
    frame: RefCell<Frame>,
    calls: Cell<u64>,
}

impl Par for ScratchPar {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        for at in 0..n {
            out.push(f(at));
        }
    }

    fn call_scratch(&self, work: &mut dyn FnMut(Alloc<'_>)) -> Scratch {
        if self.mode == Scratch::Kept {
            return Scratch::Kept;
        }
        let mut frame = self.frame.try_borrow_mut().unwrap_or_else(|_| panic!("a call's scratch was asked again inside a call (resolve.rs `expand_decls` is not re-entered)"));
        frame.scope(|f| {
            let erased = Erased::new(f);
            work(erased.handle());
        });
        self.calls.set(self.calls.get() + 1);
        Scratch::Reset
    }
}

/// What one resolve of the fixture wrote and held.
struct Ran<'o> {
    /// The crate's names record.
    record: AVec<'o, u8>,
    /// Each expansion's text and origin bytes, by its call's key.
    texts: AVec<'o, u8>,
    expansions: usize,
    /// The crate frame's peak and the bytes its growth left behind.
    peak: usize,
    abandoned: u64,
    /// The call scratch's peak (one call's high-water) and how many calls ran in it.
    scratch_peak: usize,
    scratch_calls: u64,
}

fn run<'o>(mode: Scratch, text: &str, out: Alloc<'o>) -> Ran<'o> {
    let home = Frame::new(Tag::Nameres).unwrap_or_else(|e| panic!("the crate's frame: {e:?}"));
    let erased = Erased::new(&home);
    let bump = erased.bump();
    let g = bump.alloc();
    let par = ScratchPar {
        mode,
        frame: RefCell::new(Frame::new(Tag::Expand).unwrap_or_else(|e| panic!("the call scratch: {e:?}"))),
        calls: Cell::new(0),
    };
    let crates = [input("fixture", ROOT, &[])];
    let names = resolve_crates(&crates, Sysroot::default(), &Fixture(text), &par, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    // Measured before anything else is made in the frame.
    let peak = home.peak();
    let abandoned = home.abandoned().bytes;
    let krate = names.krate(0);
    let record = crate::names_record::encode(krate, g).unwrap_or_else(|e| panic!("the names record: {e:?}"));
    let record = AVec::from_slice_in(&record[..], out);
    let mut made: AVec<'_, (ItemKey, &str, Option<&[u8]>)> = AVec::with_capacity_in(krate.expansions.len(), g);
    made.extend(krate.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, m.text, m.origins.ok()))));
    made.sort_unstable_by_key(|(call, _, _)| *call);
    let mut texts = AVec::new_in(out);
    for (call, text, origins) in made.iter() {
        texts.extend_from_slice(&call.0.0);
        texts.extend_from_slice(text.as_bytes());
        match origins {
            Some(bytes) => {
                texts.push(1);
                texts.extend_from_slice(bytes);
            }
            None => texts.push(0),
        }
    }
    let scratch_peak = par.frame.borrow().peak();
    Ran { record, texts, expansions: made.len(), peak, abandoned, scratch_peak, scratch_calls: par.calls.get() }
}

/// Expected line: `test tests::call_scratch::a_calls_work_is_reset_after_it ... ok`, printing
/// `CALL SCRATCH: calls 2000, expansions E (both), frame peak kept K B, reset R B + scratch S B,
/// ratio (R+S)/K = r, abandoned kept A B reset B B (floor 0), record bytes equal, texts equal`.
/// Expected: r well below 1 (the matcher's expansion, the rendering and the column of every
/// call are gone from the frame; what stays is the kept text, tokens, origins and lowering);
/// S is one call's work, a few KB. CONTROL: `Scratch::Kept` (the first run) is the path before
/// the hook: its peak holds every call's work, and its scratch ran no call (0 calls, 0 B).
#[test]
fn a_calls_work_is_reset_after_it() {
    let frame = Frame::new(Tag::Nameres).unwrap_or_else(|e| panic!("the test's frame: {e:?}"));
    let erased = Erased::new(&frame);
    let out = erased.handle();
    let mut text = AString::new_in(out);
    text.push_str(
        "#![no_core]
pub trait Named { fn id() -> u32; }
macro_rules! named {
    ($t:ident, $n:literal) => {
        pub struct $t { pub at: u32 }
        impl Named for $t { fn id() -> u32 { $n } }
        impl $t { pub fn at(&self) -> u32 { self.at } }
    };
}
",
    );
    for at in 0..CALLS {
        writeln!(text, "named!(S{at}, {at});").unwrap_or_else(|e| panic!("the fixture's text: {e}"));
    }
    let kept = run(Scratch::Kept, text.as_str(), out);
    let reset = run(Scratch::Reset, text.as_str(), out);
    // The control ran no call in a scratch; the piece ran every call in one.
    assert_eq!(kept.scratch_calls, 0, "the control's Par runs no work");
    assert!(reset.scratch_calls >= CALLS as u64, "every call ran in the scratch ({} of {CALLS})", reset.scratch_calls);
    assert!(kept.expansions >= CALLS, "every call expanded ({} of {CALLS})", kept.expansions);
    // Same bytes out (rule 25).
    let record_equal = kept.record[..] == reset.record[..];
    let texts_equal = kept.texts[..] == reset.texts[..];
    let ratio = (reset.peak + reset.scratch_peak) as f64 / kept.peak as f64;
    std::println!(
        "CALL SCRATCH: calls {CALLS}, expansions {} / {}, frame peak kept {} B, reset {} B + scratch {} B, ratio (R+S)/K = {ratio:.3}, abandoned kept {} B reset {} B (floor 0), record bytes equal {record_equal}, texts equal {texts_equal}",
        kept.expansions, reset.expansions, kept.peak, reset.peak, reset.scratch_peak, kept.abandoned, reset.abandoned,
    );
    assert!(record_equal, "the names record is byte-equal under both scratches");
    assert!(texts_equal, "every expansion's text and origins are byte-equal under both scratches");
    assert_eq!(kept.expansions, reset.expansions);
    assert!(ratio < 1.0, "the call's work is gone from the crate's frame: ratio {ratio:.3}");
    assert!(reset.abandoned <= kept.abandoned, "no growth is left behind that the control did not leave ({} > {})", reset.abandoned, kept.abandoned);
}
