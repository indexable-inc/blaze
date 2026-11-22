//! The test store holds the toolchain's crate records once per shelf (tests/toolchain_store):
//! the second program over it solves only its own crate, and the store is under the cargo
//! target dir, never the home.
//!
//! A wrong version fails here: a harness that re-solves the toolchain each call says
//! `Written` on the second call; one that writes the home's store fails the place check; one
//! whose records do not answer fails the `Option::Some` path from the program's root.
//!
//! It should print (times are the run's; the first call says `Read` when an earlier test
//! binary already wrote the shelf):
//!
//! ```text
//! toolchain store: first Written { crates: 63 } in 41000 ms, second Read in 900 ms, 64 crates, at <target>/tmp/blaze-test-store/<shelf hex>
//! ```

mod toolchain_store;

use std::time::Instant;

use blaze_ladder::crates::UserCrate;
use blaze_ladder::resolve::{Ns, Paths};

use toolchain_store::{Made, front_over_records};

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

#[test]
fn the_second_program_reads_the_toolchain_from_the_test_store() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let alloc = blaze_own::testing::heap().alloc();
    let files: &'static [(String, String)] = Vec::leak(vec![("/hello-world/src/main.rs".to_string(), MAIN.to_string())]);
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user: &'static UserCrate<'static> = Box::leak(Box::new(UserCrate {
        name: "hello_world",
        dir: "/hello-world/",
        root: "/hello-world/src/main.rs",
        files,
        edition,
        panic: blaze_build::Panic::Unwind,
    }));

    let start = Instant::now();
    let first = front_over_records(&shelf, user, alloc).unwrap_or_else(|e| panic!("the first program: {e}"));
    let first_ms = start.elapsed().as_millis();
    let start = Instant::now();
    let second = front_over_records(&shelf, user, alloc).unwrap_or_else(|e| panic!("the second program: {e}"));
    let second_ms = start.elapsed().as_millis();

    assert!(!second.store.starts_with(format!("{home}/.blaze")), "the test store {} is under the home's .blaze", second.store.display());
    assert!(second.store.starts_with(env!("CARGO_TARGET_TMPDIR")), "the test store {} is not under the cargo target dir", second.store.display());
    assert_eq!(second.made, Made::Read, "the second program re-solved the toolchain: its records were not found in the store");
    assert_eq!(first.store, second.store);

    let some = Paths { names: second.records }.path(second.root, false, &["Option", "Some"], Ns::Value);
    assert!(some.is_ok(), "`Option::Some` from the program's root over the store's records: {some:?}");
    assert!(std::ptr::eq(second.front.frame, second.frame), "front2 holds the frame it was given");

    println!(
        "toolchain store: first {:?} in {first_ms} ms, second {:?} in {second_ms} ms, {} crates, at {}",
        first.made,
        second.made,
        second.crate_names.len(),
        second.store.display()
    );
}
