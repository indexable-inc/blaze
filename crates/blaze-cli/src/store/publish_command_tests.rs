//! `blaze store publish` (store/publish_command.rs): its refusals, each by name before anything
//! is read or written; and the seed: after a publish, no byte of standard output or standard
//! error, no file of the origin and no file of the home's store holds it, as hex or as bytes.

use std::{format, string::String, vec::Vec as StdVec};

use blaze_remote::PublicKey;
use bstd::cx::Cx;
use bstd::entry::Status;
use bstd::fs::{Content, Dir, Durable, EntryKind, ReadWrite};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::test::BUDGET;

use super::super::published::PublishedError;
use super::super::published_e2e_tests::{MARKER, SEED, TOOLCHAIN, contains, path, plant, planted};
use super::super::StoreFailure;
use super::{Builder, Words, admit, trusted, words};
use crate::CliError;

fn parsed(line: &[&'static str]) -> Result<Words<'static>, CliError> {
    words(line.iter().copied())
}

fn refusal(result: Result<(), PublishedError>) -> PublishedError {
    result.expect_err("a refusal")
}

#[test]
fn a_developers_blaze_and_an_http_origin_are_refused_by_name() {
    let words = parsed(&["publish", "o", "--signer", "key.hex"]).expect("the words");
    let refused = refusal(admit(Builder::Developer, &words));
    assert!(matches!(refused, PublishedError::NotPublisher), "{refused:?}");
    assert_eq!(refused.to_string(), "this blaze was built without publishing: only a trusted builder's is (blaze-cli's `publish` feature, D136)");
    // The developer's refusal comes first, whatever the origin.
    let http = parsed(&["publish", "http://cache.example/o", "--signer", "key.hex"]).expect("the words");
    assert!(matches!(admit(Builder::Developer, &http), Err(PublishedError::NotPublisher)));
    for origin in ["http://cache.example/o", "https://cache.example/o"] {
        let words = parsed(&["publish", origin, "--signer", "key.hex"]).expect("the words");
        let refused = refusal(admit(Builder::Trusted, &words));
        assert!(matches!(refused, PublishedError::HttpOrigin), "{origin}: {refused:?}");
        assert!(refused.to_string().contains("only CI and the remote workers publish to a shared origin"), "{refused}");
    }
    assert!(admit(Builder::Trusted, &words).is_ok(), "a folder origin is admitted for a trusted builder");
}

#[test]
fn the_words_are_a_folder_and_one_signer() {
    assert!(parsed(&["publish", "o", "--signer", "k"]).is_ok());
    assert!(parsed(&["publish", "--signer", "k", "o"]).is_ok(), "the words in either order");
    for line in [&["publish", "o"][..], &["publish", "--signer", "k"], &["publish", "o", "--signer"], &["publish", "o", "p", "--signer", "k"], &["publish", "o", "--signer", "k", "--signer", "j"], &["publish", "o", "--key", "k"], &["pull", "o"], &[]] {
        assert!(matches!(parsed(line), Err(CliError::Usage(_))), "{line:?} taken");
    }
}

/// A world whose home `/a` holds N public records and M private ones under the toolchain's
/// root (as a trusted builder's store puts them), and whose root holds `key.hex`.
fn home_with_records(seed: u64, key_file: &[u8]) -> (Io, Cx) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("files");
    fs.root().publish(&cx, &path("key.hex"), Content::bytes(key_file), Durable::No).expect("the key file");
    fs.root().make_dir_synced(&cx, &path("o")).expect("the origin's folder");
    (io, cx)
}

fn seed_hex() -> String {
    PublicKey(SEED).to_string()
}

/// What a publish of `/a`'s records into `o` with the key file `signer` said: its result, and
/// standard output and standard error as text.
fn publish(io: &Io, cx: &Cx, signer: &'static str) -> (Result<Status, CliError>, String, String) {
    let records = planted(0..24, 8);
    let (_home, lent) = plant(io, cx, "/a", &records, false);
    let out = io.need_out().expect("out");
    let words = Words { folder: "o", signer };
    let fs = io.need_fs().expect("fs");
    let result = trusted::publish(cx, &fs, &out, &words, &lent, TOOLCHAIN);
    let (said, warned) = io.script(|host| (host.out.clone(), host.err.clone())).expect("a simulated host");
    (result, String::from_utf8_lossy(&said).into_owned(), String::from_utf8_lossy(&warned).into_owned())
}

#[test]
fn a_missing_or_bad_key_file_is_refused_and_shows_none_of_it() {
    let (io, cx) = home_with_records(81, b"not a seed: zz-secret-zz");
    let (missing, _, _) = publish(&io, &cx, "absent.hex");
    assert!(
        matches!(missing, Err(CliError::Store(StoreFailure::Published(PublishedError::Folder { what: "the signer's key file", .. })))),
        "a missing key file: {missing:?}"
    );
    let (bad, out, err) = publish(&io, &cx, "key.hex");
    let Err(bad) = bad else { panic!("a bad key file was taken") };
    assert!(matches!(bad, CliError::Store(StoreFailure::Published(PublishedError::SignerKey))), "{bad:?}");
    let shown = format!("{bad} {bad:?} {out} {err}");
    assert!(!shown.contains("zz-secret-zz"), "the key file's contents were shown: {shown}");
}

/// Every file beneath `dir` (its path from the root, its bytes).
fn files(cx: &Cx, dir: &Dir<ReadWrite>, at: &str, into: &mut StdVec<(String, StdVec<u8>)>) {
    let entries = dir.own_entries(cx).expect("a listing");
    for (name, kind) in entries.iter() {
        let named = if at.is_empty() { String::from(name) } else { format!("{at}/{name}") };
        match kind {
            EntryKind::Dir => files(cx, &dir.subdir(&path(name)), &named, into),
            EntryKind::File => {
                let mut bytes = cx.bytes();
                dir.read_all(cx, &path(name), &mut bytes).expect("a file read");
                into.push((named, bytes.as_slice().to_vec()));
            }
            _ => {}
        }
    }
}

#[test]
fn no_output_origin_file_or_store_file_holds_the_seed() {
    let hex = seed_hex();
    let (io, cx) = home_with_records(83, format!("{hex}\n").as_bytes());
    let (result, out, err) = publish(&io, &cx, "key.hex");
    assert_eq!(result.expect("the publish"), Status::OK);
    std::eprintln!("{out}");
    assert!(out.starts_with("published: set ") && out.contains(" generation 1: 24 records, ") && out.contains("; 8 left out (local); signed by "), "{out}");
    // The checkout's root (the key file, the origin) and the home (its store), each walked.
    let fs = io.need_fs().expect("files");
    let mut all = StdVec::new();
    files(&cx, &fs.root(), "", &mut all);
    let home = super::super::Home::of(&cx, &fs, Some("/a")).expect("the home");
    files(&cx, home.dir(), "/a/.blaze", &mut all);
    let origin = all.iter().filter(|(name, _)| name.starts_with("o/public/")).count();
    let store = all.iter().filter(|(name, _)| name.contains(".blaze/")).count();
    assert!(origin >= 3 && store >= 1, "the walk saw {origin} origin files and {store} store files: the grep proves nothing");
    // The control: the grep finds what it looks for, in the one file meant to hold it.
    let key_file = all.iter().find(|(name, _)| name == "key.hex").expect("the key file");
    assert!(contains(&key_file.1, hex.as_bytes()), "the grep misses the seed where it is");
    let (lower, upper) = (hex.to_lowercase(), hex.to_uppercase());
    for text in [&out, &err] {
        assert!(!text.contains(&lower) && !text.contains(&upper), "the seed in the command's output: {text}");
    }
    for (name, bytes) in all.iter().filter(|(name, _)| name != "key.hex") {
        for needle in [lower.as_bytes(), upper.as_bytes(), &SEED[..]] {
            assert!(!contains(bytes, needle), "the seed in {name}");
        }
        if name.starts_with("o/") {
            assert!(!contains(bytes, MARKER), "a private record's bytes in the origin's {name}");
        }
    }
}
