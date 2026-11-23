//! The names driver's serial Par runs each macro call's work in its call scratch, rolled back
//! once the call's results are copied out (names/metered.rs `Metered::call_scratch`, blaze-scope
//! task/fill.rs `Par::call_scratch`, lane M14 for N5's piece): the crate's frame holds what the
//! names keep, and one call's work at a time is in the scratch; the bytes written are the same.
//!
//! The fixture is one crate of [`CALLS`] calls of a `macro_rules!` that writes a struct and two
//! impls (blaze-scope tests/call_scratch.rs's shape), resolved twice, each in a fresh crate
//! frame, under a [`Metered`]: with [`CallScratch::Kept`] planted (the CONTROL, rule 23: the
//! cause removed, each call's work in the crate's frame as before the hook) and with
//! [`CallScratch::Reset`] (the driver's).
//!
//! Floor (rule 67): the crate frame's peak under `Reset` is what the names keep, no call's work
//! in it; the scratch's peak is one call's high-water. The ratio printed is
//! `(reset peak + scratch peak) / kept peak`.
//!
//! Expected (`cargo test -p blaze-cli --bin blaze -- scratch_tests --nocapture`):
//!   `metered call scratch: calls 2000, expansions E / E, crate frame peak kept K B, reset R B +
//!   scratch S B over C calls, ratio (R+S)/K = r, record bytes equal true, texts equal true`,
//!   with r < 1 and C >= 2000; the control's scratch ran 0 calls; and the profile row of the
//!   `Reset` run names `call scratch peak S over C calls`.

use core::fmt::Write as _;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, Erased};
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};
use bstd::mem::{Frame, Tag};

use super::{CallScratch, Metered};
use crate::phases::byte_rows::Label;

/// How many calls the fixture writes.
const CALLS: usize = 2000;

/// The fixture's root file.
const ROOT: &str = "/fixture/lib.rs";

const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

/// The one crate's one file, made in the test's frame.
struct Fixture<'t>(&'t str);

impl<'a, 't: 'a> SourceTexts<'a> for Fixture<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == ROOT).then_some(self.0)
    }
}

/// What one resolve of the fixture wrote and held.
struct Ran<'o> {
    /// The crate's names record.
    record: AVec<'o, u8>,
    /// Each expansion's call key, text and origin bytes, in call order.
    texts: AVec<'o, u8>,
    expansions: usize,
    /// The crate frame's peak, measured before anything else is made in it.
    peak: usize,
    /// The call scratch's peak and calls, as `finish` reports them.
    scratch_peak: u64,
    scratch_calls: u64,
    /// The profile's row.
    row: AString<'o>,
}

fn run<'o>(mode: CallScratch, text: &str, out: Alloc<'o>) -> Ran<'o> {
    let home = Frame::new(Tag::Nameres).unwrap_or_else(|e| panic!("the crate's frame: {e:?}"));
    let erased = Erased::new(&home);
    let bump = erased.bump();
    let g = bump.alloc();
    let scratch = Frame::new(Tag::Expand).unwrap_or_else(|e| panic!("the call scratch: {e:?}"));
    let metered = Metered::new(&home, scratch, mode);
    let crates = [CrateInput {
        name: "fixture",
        key: CrateItemKey(ContentId::of("blaze-cli metered call scratch test crate", b"fixture")),
        edition: Edition::E2021,
        implicit: RUST,
        root: ROOT,
        shown_from: "/",
        features: &[],
        deps: &[],
        cfg: &[],
        script: None,
    }];
    let names = resolve_crates(&crates, Sysroot::default(), &Fixture(text), &metered, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let peak = home.peak();
    let bytes = metered.finish(Label::of(format_args!("fixture")));
    let krate = names.krate(0);
    let record = blaze_scope::names_record::encode(krate, g).unwrap_or_else(|e| panic!("the names record: {e:?}"));
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
    let mut row = AString::new_in(out);
    write!(row, "{bytes}").unwrap_or_else(|e| panic!("the profile's row: {e}"));
    Ran { record, texts, expansions: made.len(), peak, scratch_peak: bytes.scratch_peak, scratch_calls: bytes.scratch_calls, row }
}

#[test]
fn the_metered_call_scratch_resets_after_each_call() {
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
    let kept = run(CallScratch::Kept, text.as_str(), out);
    let reset = run(CallScratch::Reset, text.as_str(), out);
    // The control ran no call in the scratch; the driver's ran every call in it.
    assert_eq!(kept.scratch_calls, 0, "the control's Metered runs no work in its scratch");
    assert!(reset.scratch_calls >= CALLS as u64, "every call ran in the scratch ({} of {CALLS})", reset.scratch_calls);
    assert!(kept.expansions >= CALLS, "every call expanded ({} of {CALLS})", kept.expansions);
    // Same bytes out (rule 25).
    let record_equal = kept.record[..] == reset.record[..];
    let texts_equal = kept.texts[..] == reset.texts[..];
    let ratio = (reset.peak as u64 + reset.scratch_peak) as f64 / kept.peak as f64;
    std::println!(
        "metered call scratch: calls {CALLS}, expansions {} / {}, crate frame peak kept {} B, reset {} B + scratch {} B over {} calls, ratio (R+S)/K = {ratio:.3}, record bytes equal {record_equal}, texts equal {texts_equal}",
        kept.expansions, reset.expansions, kept.peak, reset.peak, reset.scratch_peak, reset.scratch_calls,
    );
    assert!(record_equal, "the names record is byte-equal under both scratches");
    assert!(texts_equal, "every expansion's text and origins are byte-equal under both scratches");
    assert_eq!(kept.expansions, reset.expansions);
    assert!(reset.peak < kept.peak, "the call's work is gone from the crate's frame ({} B, the control's {} B)", reset.peak, kept.peak);
    assert!(ratio < 1.0, "the crate frame and the scratch together hold less than every call's work kept: ratio {ratio:.3}");
    // The profile's row names the scratch.
    let mut named = AString::new_in(out);
    write!(named, "call scratch peak {} over {} calls", reset.scratch_peak, reset.scratch_calls).unwrap_or_else(|e| panic!("the row's words: {e}"));
    assert!(reset.row.as_str().contains(named.as_str()), "the row names the call scratch: {}", reset.row.as_str());
}
