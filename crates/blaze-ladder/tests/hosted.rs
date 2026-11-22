//! The hosted rungs (`7d-hosted` on): each is started at bstd's hosted entry with a real host
//! table from bstd's `Hosting`, its calls recorded, and must write exactly its `expected`
//! standard output through the table's `write` and end with its `expected` status. Its own test
//! binary: the program runs on this process's thread.

mod rungs;

use core::ffi::c_void;
use core::ptr::NonNull;

use blaze_ladder::refusal::SourceFile;
use bstd::hosting::{Call, Hold, Holding, Hosting};
use bstd::io::Io;

/// The hosted rungs that run today.
const HOSTED: &[&str] = &["7d-hosted"];

/// The host's stream number for standard output (crates/blaze-std/src/wire.rs).
const STREAM_OUT: u32 = 1;

#[test]
fn every_hosted_rung_writes_its_expected_output_through_the_table() {
    for rung in HOSTED {
        let dir = rungs::ladder().join(rung);
        let (status, stdout) = rungs::expected(&dir);
        let files = rungs::sources(&dir);
        let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
        let shared = blaze_own::testing::heap().shared();
        let hosting = Hosting::holding(Io::sim(1), Vec::new(), Vec::new(), 4096, Holding::of(Hold::Record), shared);
        let table = hosting.table();
        let host = NonNull::from(&table).cast::<c_void>();
        let got = match blaze_ladder::run::run(&views, "src/main.rs", Some(host), rungs::x::overflow_of(&dir), shared) {
            Ok(got) => got,
            Err(error) => panic!("{rung}: {error}"),
        };
        let mut written = Vec::new();
        for called in hosting.record().calls {
            if let Call::Write { stream: STREAM_OUT, bytes } = called.call {
                written.extend_from_slice(&bytes);
            }
        }
        assert_eq!((got, String::from_utf8_lossy(&written)), (status, String::from_utf8_lossy(&stdout)), "{rung}");
    }
}
