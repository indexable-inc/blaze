//! Decision D111 through the BODY host (the gap the D96 lane named, 2026-11-16): a workspace
//! member's body-local proc-macro call whose run read a file outside the member's own
//! directory gives exactly one finding naming the call, through the one filing the item host
//! uses (blaze-scope left_reads.rs `file_left_reads`, rule 30).
//!
//! - The body call `macros::sol!()`, written 16 bytes into the body of an item whose text
//!   starts at byte 100 of `/0/crates/m/src/lib.rs`, its run having read its own
//!   `crates/m/abi/own.json` and the sibling's `crates/sibling/x.json` (twice), is ONE finding:
//!   at byte 116, `macros::sol`, fn-like, `/0/crates/sibling/x.json`.
//! - Control 1: the same call at item level (the item host's filing, given the same call's
//!   place and read list) gives the same finding, byte for byte.
//! - Control 2: a call whose run read only the member's own file gives none.
//! - The body lowered twice (front2 `lower_again`) files the same finding again; the sink keeps
//!   one (blaze-cli names/include_outside.rs `findings`: sorted, without repeats), done here
//!   the same way: exactly one (rule 77).
//!
//! Control (rule 8, once): make `Sourced::file_left_reads` (body_host.rs) return 0 without
//! filing: the assert "the body call's read outside the member is one finding" fails.
//!
//! Staged by the d111-body-host writer (wiring/blaze-ladder/d111-body-host-1.md); NEVER
//! COMPILED. Expected (`lanecheck.sh blaze-ladder test '--test front2_d111_body_host'`):
//! "test blaze-ladder: exit 0; 1 passed, 0 failed"; with --nocapture it prints
//! "body: 1 finding, `macros::sol!` at 116 reads /0/crates/sibling/x.json; item: equal; no outside read: 0".

use std::string::{String, ToString};
use std::sync::Mutex;
use std::vec::Vec;

use blaze_expand::seen::SeenRoot;
use blaze_expand::{ReadAnswer, ReadKind, Reads};
use blaze_ladder::front2::body_host::{BodySources, Sourced};
use blaze_ladder::front2::body_left::{BodyPlace, file_body_left_reads};
use blaze_lower::call_reads::CallReads;
use blaze_own::{AString, Alloc};
use blaze_scope::crate_env::{CrateEnv, EnvFacts};
use blaze_scope::include_place::LeftPackage;
use blaze_scope::left_reads::{CallAt, file_left_reads};
use blaze_scope::resolve::SourceTexts;
use blaze_surface::{CallShape, Range};

/// One finding told: (calling file, byte, macro as written, shape, file read).
type Told = (String, u32, String, CallShape, String);

/// Crate 0 is the member `crates/m` of a workspace (tree 0); the findings told.
struct Member {
    env: CrateEnv<'static>,
    told: Mutex<Vec<Told>>,
}

impl SourceTexts<'static> for Member {
    fn read(&self, _: usize, _: &str) -> Option<&'static str> {
        None
    }
    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'static>> {
        (krate == 0).then_some(&self.env)
    }
    fn left_package(&self, _: usize, left: LeftPackage<'_>) {
        let told = (left.from.to_string(), left.at, left.call.to_string(), left.shape, left.path.to_string());
        self.told.lock().unwrap_or_else(|p| panic!("told: {p}")).push(told);
    }
}

impl Member {
    fn take(&self) -> Vec<Told> {
        core::mem::take(&mut *self.told.lock().unwrap_or_else(|p| panic!("told: {p}")))
    }
}

fn written(text: &str, alloc: Alloc<'static>) -> AString<'static> {
    let mut out = AString::new_in(alloc);
    out.push_str(text);
    out
}

#[test]
fn a_body_call_s_read_outside_the_member_is_one_finding_the_item_host_s() {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let facts = EnvFacts {
        crate_name: "user",
        package: Some(("user", "0.1.0")),
        bin: None,
        seen: SeenRoot::member("crates/m").unwrap_or_else(|e| panic!("{e}")),
        read_folder: "/0/crates/m/",
        build_script: false,
        script: None,
        test_or_bench: false,
        bin_exes: blaze_scope::crate_env::BinNames::NONE,
        meta: None,
    };
    let env = CrateEnv::of(facts, alloc).unwrap_or_else(|e| panic!("the member's environment: {e:?}"));
    let member: &'static Member = std::boxed::Box::leak(std::boxed::Box::new(Member { env, told: Mutex::new(Vec::new()) }));
    let sources: &dyn BodySources = std::boxed::Box::leak(std::boxed::Box::new(Sourced(member)));

    // The body's read list: `sol!`'s run (reads 0..4), then `own!`'s (4..5).
    let mut reads = Reads::new_in(alloc);
    for subject in ["env:CARGO_MANIFEST_DIR", "file:crates/m/abi/own.json", "file:crates/sibling/x.json", "file:crates/sibling/x.json", "file:crates/m/abi/own.json"] {
        reads.push(ReadKind::ProcMacro, subject, ReadAnswer::Absent).unwrap_or_else(|e| panic!("push: {e:?}"));
    }
    let body = Some(BodyPlace { from: "/0/crates/m/src/lib.rs", start: 100, len: 60 });
    let sol = CallReads { at: Some(Range { start: 16, end: 30 }), path: written("macros::sol", alloc), shape: CallShape::FnLike, reads: 0..4 };
    let own = CallReads { at: Some(Range { start: 33, end: 45 }), path: written("macros::own", alloc), shape: CallShape::FnLike, reads: 4..5 };

    // The body host: one finding, at the call, naming it.
    assert_eq!(file_body_left_reads(sources, 0, body, core::slice::from_ref(&sol), &reads, alloc), 1);
    let body_told = member.take();
    let want: Told = ("/0/crates/m/src/lib.rs".to_string(), 116, "macros::sol".to_string(), CallShape::FnLike, "/0/crates/sibling/x.json".to_string());
    assert_eq!(body_told, [want.clone()], "the body call's read outside the member is one finding");

    // Control 1: the same call at item level, through the item host's filing.
    let item_call = CallAt { from: "/0/crates/m/src/lib.rs", at: 116, call: "macros::sol", shape: CallShape::FnLike };
    assert_eq!(file_left_reads(member, 0, item_call, &reads, 0..4, alloc), 1);
    assert_eq!(member.take(), body_told, "the item host gives the body host's finding, byte for byte");

    // Control 2: a call that read only the member's own file gives none.
    assert_eq!(file_body_left_reads(sources, 0, body, core::slice::from_ref(&own), &reads, alloc), 0);
    assert!(member.take().is_empty(), "no outside read, no finding");

    // Lowered twice: the sink keeps one (rule 77).
    file_body_left_reads(sources, 0, body, core::slice::from_ref(&sol), &reads, alloc);
    file_body_left_reads(sources, 0, body, core::slice::from_ref(&sol), &reads, alloc);
    let mut twice = member.take();
    twice.sort();
    twice.dedup();
    assert_eq!(twice, [want], "two passes over one body are one finding at the sink");

    std::println!("body: 1 finding, `{}!` at {} reads {}; item: equal; no outside read: 0", body_told[0].2, body_told[0].1, body_told[0].4);
}
