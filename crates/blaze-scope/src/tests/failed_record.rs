//! The names record carries each failed call's typed reason (implicit-main-reason; rules 19, 26,
//! 71): a reader of the record alone (a dependent, a body's check, front2 from the store) finds
//! the call a name is missing behind ([`crate::names::Why::Refused`]'s `call`) and gets the
//! macro's path, whether it is the implicit `::std::main` of D104, and the matcher's own reason
//! ([`blaze_expand::Failed`]), equal to the solve's note; never a string, never nothing.
//!
//! The Reference, "Macros By Example", "Transcribing": "tries each macro rule in turn ... If no
//! rule matches, an error is reported": the call is an error, and the error is what the record
//! keeps. CONTROL (rule 8): with names_record.rs's failed-calls section left out of `encode`
//! (an empty section), `the_failed_calls_reason_round_trips_through_the_names_record` fails
//! with "a failed call's row is in the record".

use std::format;

use blaze_own::Alloc;

use super::entry::{Texts, input};
use crate::graph::Sysroot;
use crate::names_record::{NamesRecord, NamesRecordError, Verified, encode};
use crate::resolve::{CrateInput, ExpandNote, NamesNote, aligned, resolve_crates};
use crate::task::Serial;

/// A macro that writes a fn from `fn NAME`, called with `struct foo`: no rule matched.
const MAKER: &str = "#![no_core]
macro_rules! make {
    (fn $n:ident) => { pub fn $n() {} };
}
make!(struct foo);
pub fn here() {}
";

static TEXTS: Texts = Texts(&[&[("/maker/lib.rs", MAKER)]]);

/// The failed `make!(struct foo)` is a row of the crate's names record: read back by its call
/// key it is the solve's own note, path, flag and reason, in equal words; a key that is no
/// failed call answers `None`; and the record of an older version is refused by name (rule 26:
/// an old home is reported and made again, never read). Passing prints
/// "test tests::failed_record::the_failed_calls_reason_round_trips_through_the_names_record ... ok".
#[test]
fn the_failed_calls_reason_round_trips_through_the_names_record() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let crates: [CrateInput<'static>; 1] = [input("maker", "/maker/lib.rs", &[])];
    let names = resolve_crates(&crates, Sysroot::default(), &TEXTS, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let c = names.crates()[0];
    let (call, path, implicit, failed) = c
        .notes
        .iter()
        .find_map(|note| match *note {
            NamesNote::Expand { call, note: ExpandNote::Failed { path, implicit, failed }, .. } => Some((call, path, implicit, failed)),
            _ => None,
        })
        .expect("`make!(struct foo)` failed, and the solve noted it");
    let written = encode(c, alloc).unwrap_or_else(|e| panic!("encode: {e:?}"));
    let bytes = aligned(&written, bump);
    let record = NamesRecord::open(Verified::check(bytes).unwrap_or_else(|e| panic!("check: {e:?}")));
    let row = record.failed(call, bump).unwrap_or_else(|e| panic!("failed: {e:?}")).expect("a failed call's row is in the record");
    assert_eq!((row.call, row.path, row.implicit), (call, path, implicit));
    assert_eq!(row.path, "make");
    assert!(!row.implicit);
    assert_eq!(row.why, crate::call_refusal::CallRefusal::Failed(failed), "the reason read back is the solve's");
    assert_eq!(format!("{}", row.why), format!("{failed}"));
    assert!(format!("{}", row.why).contains("no rule of the macro's 1 matched"), "{}", row.why);

    // The control of the lookup: a key that is no failed call.
    assert!(record.failed(c.root, bump).unwrap_or_else(|e| panic!("failed: {e:?}")).is_none());

    // Deterministic: a second encode writes equal bytes.
    let again = encode(c, alloc).unwrap_or_else(|e| panic!("encode: {e:?}"));
    assert_eq!(written[..], again[..]);

    // An old home's record (version 1, before the failed-calls section) is refused.
    let mut old = std::vec::Vec::from(&written[..]);
    old[4..8].copy_from_slice(&1u32.to_le_bytes());
    let old = aligned(&old, bump);
    assert!(matches!(Verified::check(old), Err(NamesRecordError::NotThisRecord)));
}
