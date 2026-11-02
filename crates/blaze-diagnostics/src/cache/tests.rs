use std::{collections::HashMap, sync::Arc};

use base_db::{Crate, Db, Env};
use blaze_format::Aligned;
use blaze_ids::ContentId;
use hir_def::deps::{self as deps, ImageBuf, ImageStore};
use hir_expand::fixture;
use hir_ty_borrowck::Mode;
use parking_lot::Mutex;

use span::FileId;

use super::*;
use crate::{
    emit::{Sources, Style},
    model::{Child, Code, Expansion, Level, MacroKind, Sublevel},
};

const BUILD: ContentId = ContentId([7; 32]);

fn flags(it: &[&str]) -> Vec<String> {
    it.iter().map(|&it| it.to_owned()).collect()
}

fn inputs(flags: &[String]) -> Inputs<'_> {
    Inputs { lint_flags: flags, keep_duplicates: false, borrowck: Mode::Polonius }
}

#[test]
fn the_key_changes_with_each_input() {
    let image = ContentId([1; 32]);
    let base = flags(&["--cap-lints", "allow", "-W", "unused"]);
    let key_of = |image, inputs: &Inputs<'_>| key(image, inputs);
    let was = key_of(image, &inputs(&base));
    assert_eq!(key_of(image, &inputs(&base)), was, "a key is a function of its inputs");
    assert_ne!(key_of(ContentId([2; 32]), &inputs(&base)), was, "the image's key");
    let other = flags(&["--cap-lints", "warn", "-W", "unused"]);
    assert_ne!(key_of(image, &inputs(&other)), was, "a lint flag's value");
    let reordered = flags(&["-W", "unused", "--cap-lints", "allow"]);
    assert_ne!(key_of(image, &inputs(&reordered)), was, "the flags' order, which rustc reads");
    let fewer = flags(&["--cap-lints", "allow"]);
    assert_ne!(key_of(image, &inputs(&fewer)), was, "a flag left out");
    let joined = flags(&["--cap-lintsallow", "-W", "unused"]);
    assert_ne!(key_of(image, &inputs(&joined)), was, "flags are framed, not concatenated");
    let duplicates = Inputs { keep_duplicates: true, ..inputs(&base) };
    assert_ne!(key_of(image, &duplicates), was, "keep_duplicates");
    for mode in [Mode::Nll, Mode::Off] {
        let other = Inputs { borrowck: mode, ..inputs(&base) };
        assert_ne!(key_of(image, &other), was, "the borrow checker's mode {mode:?}");
        assert_ne!(bodies_key(image, &[], mode), bodies_key(image, &[], Mode::Polonius), "{mode:?}");
    }
    let bodies = bodies_key(image, &[], Mode::Polonius);
    assert_ne!(bodies_key(ContentId([2; 32]), &[], Mode::Polonius), bodies, "the image's key");
    let (a, b) = (ContentId([3; 32]), ContentId([4; 32]));
    assert_ne!(bodies_key(image, &[a], Mode::Polonius), bodies, "a dependency's check key");
    assert_ne!(bodies_key(image, &[b], Mode::Polonius), bodies_key(image, &[a], Mode::Polonius), "its value");
    assert_ne!(bodies_key(image, &[a, b], Mode::Polonius), bodies_key(image, &[b, a], Mode::Polonius), "their order");
    assert_ne!(bodies, was, "the two values have two keys");
}

const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
fn main() { lib::f(); }

//- /lib.rs crate:lib deps:core new_source_root:library env:CARGO_MANIFEST_DIR=/reg/lib-1.0.0
pub fn f() -> u32 { let x: u32 = "one"; x }

//- /core.rs crate:core new_source_root:library
#[lang = "sized"]
pub trait Sized {}
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// The outcome key of `name` in `fixture`, for a crate whose image and bodies read nothing (each
/// filed under its key) and whose dependencies' check keys are given alike.
fn outcome_key(fixture: &str, name: &str) -> ContentId {
    let fx = fixture::db(fixture);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    let krate = crate_named(&db, name);
    let image = deps::keys(&db, &[krate], BUILD)[&krate];
    key(bodies_key(image, &[ContentId([3; 32])], Mode::Polonius), &inputs(&flags(&["--cap-lints", "allow"])))
}

#[test]
fn a_dependents_text_does_not_change_the_key() {
    let was = outcome_key(FIXTURE, "lib");
    assert_eq!(outcome_key(&FIXTURE.replace("fn main() {", "fn main() { let _ = 1;"), "lib"), was);
    let core = outcome_key(FIXTURE, "core");
    assert_eq!(outcome_key(&FIXTURE.replace("\"one\"", "\"two\""), "core"), core);
}

#[test]
fn an_edit_to_the_dependency_misses() {
    let was = outcome_key(FIXTURE, "lib");
    assert_ne!(outcome_key(&FIXTURE.replace("\"one\"", "\"two\""), "lib"), was);
    assert_ne!(outcome_key(&FIXTURE.replace("pub trait Sized {}", "pub trait Sized {}\npub struct S;"), "lib"), was);
}

fn span(file: &str, lo: u32, hi: u32) -> Span {
    Span { file: file.to_owned(), lo, hi, expansion: None }
}

/// A diagnostic in `file` touching each kind of string an outcome rewrites.
fn diag_in(file: &str, out: &str) -> Diag {
    let mut called = span(file, 4, 9);
    called.expansion = Some(Box::new(Expansion {
        call_site: span(file, 1, 3),
        kind: MacroKind::Bang,
        name: "include_str".to_owned(),
        def_site: Some(span(file, 0, 1)),
        opaque: false,
    }));
    let mut diag = Diag::error(Some("E0308"), "mismatched types", called);
    diag.span.labels.push((span(file, 4, 9), "expected `u32`".to_owned()));
    diag.children.push(Child {
        level: Sublevel::Note,
        message: format!("couldn't read `{out}/gen.rs`"),
        span: MultiSpan::of(span(file, 2, 3)),
        once: false,
    });
    diag
}

#[test]
fn an_outcome_reads_back_in_another_root() {
    // A registry crate outside the workspace: its files are named whole.
    let here = Paths::new(Some("/reg/lib-1.0.0".into()), Some("/w/target/debug/build/lib-1/out".into()));
    let diags = vec![diag_in("/reg/lib-1.0.0/src/lib.rs", "/w/target/debug/build/lib-1/out")];
    let bytes = encode(&diags, &here, "/home/a/work").unwrap();
    assert!(!bytes.windows(4).any(|it| it == b"/reg"), "no path of this machine is kept");
    let there = || Paths::new(Some("/cargo/reg/lib-1.0.0/".into()), Some("/x/out".into()));
    let read = decode(&bytes, there, "/home/b/work").unwrap();
    assert_eq!(read.diags, vec![diag_in("/cargo/reg/lib-1.0.0/src/lib.rs", "/x/out")]);
    assert!(read.failed);

    // A vendored crate below the workspace: named from the root, in both.
    let here = Paths::new(Some("/home/a/work/vendor/lib".into()), None);
    let diags = vec![diag_in("vendor/lib/src/lib.rs", "nothing")];
    let bytes = encode(&diags, &here, "/home/a/work/").unwrap();
    let there = || Paths::new(Some("/y/vendor/lib".into()), None);
    assert_eq!(decode(&bytes, there, "/y").unwrap().diags, diags);
    // Read in a checkout where the crate is not below the root, it is named whole.
    let elsewhere = || Paths::new(Some("/z/lib".into()), None);
    assert_eq!(decode(&bytes, elsewhere, "/y").unwrap().diags, vec![diag_in("/z/lib/src/lib.rs", "nothing")]);
}

#[test]
fn an_outcome_naming_another_machine_path_is_refused() {
    let paths = Paths::new(Some("/reg/lib-1.0.0".into()), None);
    let root = "/home/a/work";
    let refused = |diag: Diag| encode(&[diag], &paths, root).unwrap_err();
    // Another crate's file: a macro of the standard library.
    let mut diag = Diag::error(None, "x", span("/reg/lib-1.0.0/src/lib.rs", 0, 1));
    diag.span.primary[0].expansion = Some(Box::new(Expansion {
        call_site: span("/reg/lib-1.0.0/src/lib.rs", 0, 1),
        kind: MacroKind::Bang,
        name: "panic".to_owned(),
        def_site: Some(span("/toolchain/lib/rustlib/src/rust/library/core/src/macros/mod.rs", 0, 1)),
        opaque: false,
    }));
    assert!(matches!(refused(diag), Refused::MachinePath { .. }));
    // A sibling package whose directory starts with the crate's.
    let diag = Diag::error(None, "see /reg/lib-1.0.0-extra/src/a.rs", span("/reg/lib-1.0.0/a.rs", 0, 1));
    assert_eq!(refused(diag), Refused::MachinePath { path: "/reg/lib-1.0.0-extra/src/a.rs".to_owned() });
    // The workspace root in a text.
    let diag = Diag::error(None, "in /home/a/work/src", span("/reg/lib-1.0.0/a.rs", 0, 1));
    assert!(matches!(refused(diag), Refused::MachinePath { .. }));
    // A file the front end has no path for.
    assert!(matches!(refused(Diag::error(None, "x", span("<file 3>", 0, 1))), Refused::NoPath { .. }));
    assert_eq!(refused(Diag::error(None, "a\0b", span("/reg/lib-1.0.0/a.rs", 0, 1))), Refused::Nul);
    // A crate with no package directory keeps no span.
    let none = Paths::new(None, None);
    let error = encode(&[Diag::error(None, "x", span("/reg/lib-1.0.0/a.rs", 0, 1))], &none, root);
    assert!(matches!(error, Err(Refused::MachinePath { .. })));
    // An outcome of no diagnostics holds no path.
    assert!(encode(&[], &none, root).is_ok());
}

#[test]
fn an_outcome_naming_a_path_the_reader_lacks_is_an_error() {
    let paths = Paths::new(Some("/reg/lib".into()), Some("/out".into()));
    let bytes = encode(&[diag_in("/reg/lib/src/lib.rs", "/out")], &paths, "/w/root/a").unwrap();
    assert!(decode(&bytes, || Paths::new(Some("/reg/lib".into()), None), "/w").is_err());
    assert!(decode(&bytes, || Paths::new(None, Some("/out".into())), "/w").is_err());
}

#[test]
fn damaged_outcomes_are_refused_within_bounds() {
    let paths = || Paths::new(Some("/reg/lib".into()), Some("/out".into()));
    let diags = vec![diag_in("/reg/lib/src/lib.rs", "/out"), Diag::lint("dead_code", "unused", span("/reg/lib/a.rs", 1, 2))];
    let bytes = encode(&diags, &paths(), "/w/root/a").unwrap();
    assert_eq!(decode(&bytes, paths, "/w").unwrap().diags.len(), 2);
    for len in 0..bytes.len() {
        assert!(decode(&bytes[..len], paths, "/w").is_err(), "cut at {len}");
    }
    for at in 0..HEADER {
        for bit in 0..8 {
            let mut damaged = bytes.clone();
            damaged[at] ^= 1 << bit;
            assert!(decode(&damaged, paths, "/w").is_err(), "byte {at} bit {bit}");
        }
    }
    let empty = encode(&[], &paths(), "/w").unwrap();
    let mut failed = empty.clone();
    failed[4] = 1;
    assert!(decode(&failed, paths, "/w").is_err(), "failed with no diagnostics");
    let mut padded = empty.clone();
    padded.extend_from_slice(b" ");
    assert!(decode(&padded, paths, "/w").is_err(), "a body past its length");
    // Flips in the body decode or are refused; none panics.
    for at in HEADER..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x20;
        let _ = decode(&damaged, paths, "/w");
    }
}

#[test]
fn an_empty_outcome_reads_without_allocating() {
    let empty = encode(&[], &Paths::new(None, None), "/w").unwrap();
    assert_eq!(empty.len(), HEADER + 2);
    let flags = flags(&["--cap-lints", "allow"]);
    let inputs = inputs(&flags);
    let _ = *PRODUCER;
    let read = blaze_alloc_count::assert_at_most(0, "an empty outcome read and its key made", || {
        let _ = key(ContentId([1; 32]), &inputs);
        decode(&empty, || unreachable!("an empty outcome asks for no paths"), "/w")
    });
    assert_eq!(read, Ok(Outcome { failed: false, diags: Vec::new() }));
}

/// The crates of `fixture` named under `dir`, checked as a build checks them from `root`.
fn checked(fixture: &str, dir: &str, root: &str) -> (Db, HashMap<FileId, String>, Vec<Diag>) {
    checked_with(fixture, dir, root, &[])
}

/// As [`checked`], in a process with the variables `vars`.
fn checked_with(
    fixture: &str,
    dir: &str,
    root: &str,
    vars: &[(&str, &str)],
) -> (Db, HashMap<FileId, String>, Vec<Diag>) {
    let fx = fixture::db(fixture);
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    if !vars.is_empty() {
        base_db::set_process_env(&mut db, Env::from_iter(vars.iter().map(|&(name, value)| (name.into(), value.into()))));
    }
    let names: HashMap<FileId, String> = files
        .iter()
        .zip(fixture.lines().filter_map(|it| it.strip_prefix("//- /")).map(|it| it.split_whitespace().next().unwrap()))
        .map(|(file, path)| (file.file_id(&db), format!("{dir}/{path}")))
        .collect();
    let diags = hir::attach_db(&db, || {
        let lib = hir::Crate::from(crate_named(&db, "lib"));
        crate::check::check_crate(&db, lib, Files::new(&names, root), &Options::default())
    });
    (db, names, diags)
}

fn stderr(db: &Db, names: &HashMap<FileId, String>, root: &str, diags: &[Diag]) -> String {
    let sources = Sources::of(
        names.iter().map(|(file, name)| (Files::relative(name, root).to_owned(), db.file_text(*file).text(db).as_str().to_owned())),
    );
    diags.iter().map(|it| crate::emit::written(it, &sources, Style::default()).unwrap()).collect()
}

#[test]
fn a_served_outcome_renders_byte_identical() {
    // An early lint's warning: the error leaves out the late lints, `dead_code` among them.
    let fixture = FIXTURE.replace("pub fn f()", "pub fn g() -> u32 { (1) }\npub fn f()");
    let (db, names, diags) = checked(&fixture, "/reg/lib-1.0.0", "/home/a/work");
    assert!(diags.iter().any(|it| it.code == Some(Code::Error("E0308".to_owned()))), "{diags:?}");
    let lib = crate_named(&db, "lib");
    let bytes = encode(&diags, &Paths::of(&db, lib), "/home/a/work").unwrap();

    // The same checkout reads back what it wrote.
    let read = decode(&bytes, || Paths::of(&db, lib), "/home/a/work").unwrap();
    assert_eq!(read, Outcome::new(diags.clone()));
    assert_eq!(stderr(&db, &names, "/home/a/work", &read.diags), stderr(&db, &names, "/home/a/work", &diags));

    // Another checkout, with the crate at another path, reads what its own check gives.
    let moved = fixture.replace("CARGO_MANIFEST_DIR=/reg/lib-1.0.0", "CARGO_MANIFEST_DIR=/cargo/lib-1.0.0");
    let (there, there_names, there_diags) = checked(&moved, "/cargo/lib-1.0.0", "/home/b");
    let read = decode(&bytes, || Paths::of(&there, crate_named(&there, "lib")), "/home/b").unwrap();
    assert_eq!(read.diags, there_diags);
    assert_eq!(stderr(&there, &there_names, "/home/b", &read.diags), stderr(&there, &there_names, "/home/b", &there_diags));
    assert!(read.failed);
    assert!(read.diags.iter().any(|it| it.level == Level::Warning), "the parentheses' lint: {:?}", read.diags);
}

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
#[derive(Default)]
struct Memory {
    values: Mutex<blaze_hash::HashMap<ContentId, Arc<Aligned>>>,
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(self.values.lock().get(key).map(|it| it.clone() as ImageBuf))
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        let mut values = self.values.lock();
        match values.get(&key) {
            Some(kept) if kept.bytes() != value.as_slice() => Err(format!("{key:?} holds other bytes")),
            Some(_) => Ok(()),
            None => {
                values.insert(key, Arc::new(Aligned::copy_from(&value)));
                Ok(())
            }
        }
    }

    fn reads_now(&self, _: &Env, _: &[u8]) -> Result<ContentId, String> {
        Err("the fixture's crates read nothing".to_owned())
    }
}

/// `core`'s and `lib`'s images made and kept in `store` from `db`, with the keys they are filed
/// under.
fn imaged(db: &mut Db, store: &Memory) -> (ContentId, ContentId) {
    let all = base_db::all_crates(db).to_vec();
    let served = deps::serve(db, &all, BUILD, store, deps::Mode::Serve).unwrap();
    assert!(served.hits.is_empty());
    let stored = deps::store(db, &served, store).unwrap();
    (stored.filed[&crate_named(db, "core")], stored.filed[&crate_named(db, "lib")])
}

/// A second database of `fixture`, its images served from `store`.
fn served_db(fixture: &str, vars: &[(&str, &str)], store: &Memory) -> Db {
    let fx = fixture::db(fixture);
    let _ = fx.files().to_vec();
    let mut db = fx.into_db();
    if !vars.is_empty() {
        base_db::set_process_env(&mut db, Env::from_iter(vars.iter().map(|&(name, value)| (name.into(), value.into()))));
    }
    let all = base_db::all_crates(&db).to_vec();
    let images = deps::serve(&mut db, &all, BUILD, store, deps::Mode::Serve).unwrap();
    assert!(images.hits.contains(&crate_named(&db, "lib")));
    db
}

#[test]
fn a_served_dependency_reads_its_outcome_and_bodies_in_a_second_database() {
    let store = Memory::default();
    let flags = flags(&["--cap-lints", "allow"]);
    let inputs = inputs(&flags);
    let root = "/home/a/work";

    // The first build: images, outcomes and bodies made and kept, `core` before `lib`.
    let (mut db, _, diags) = checked(FIXTURE, "/reg/lib-1.0.0", root);
    let (core_image, lib_image) = imaged(&mut db, &store);
    let (core, lib) = (crate_named(&db, "core"), crate_named(&db, "lib"));
    let checked = [
        Checked { krate: lib, image: lib_image, inputs, diags: &diags },
        Checked { krate: core, image: core_image, inputs, diags: &[] },
    ];
    let kept = super::store(&db, &checked, &Served::default(), root, &store).unwrap();
    assert!(kept.refused.is_empty(), "{:?}", kept.refused);
    assert_eq!(kept.keys.len(), 2);
    assert_ne!(kept.keys[&lib], kept.keys[&core]);
    // Kept again, the same bytes: a key names one value.
    let again = super::store(&db, &checked, &Served::default(), root, &store).unwrap();
    assert_eq!(again.keys, kept.keys);
    // `lib` alone: its dependency has no check key, so neither has it.
    let alone = super::store(&db, &checked[..1], &Served::default(), root, &store).unwrap();
    assert_eq!(alone.refused, [(lib, Refused::Dependency { name: "core".to_owned() })]);
    assert!(alone.keys.is_empty());

    // A second database serves both, under the same check keys.
    let mut second = served_db(FIXTURE, &[], &store);
    let (core2, lib2) = (crate_named(&second, "core"), crate_named(&second, "lib"));
    assert_eq!(image_key(&second, lib2), Some(lib_image));
    let checks = fresh(&mut second, &[(lib2, inputs), (core2, inputs)], &store);
    assert!(checks.has(lib2) && checks.has(core2));
    assert_eq!((checks.keys[&lib2], checks.keys[&core2]), (kept.keys[&lib], kept.keys[&core]));
    let outcome = read(&second, &checks, lib2, root).unwrap().unwrap();
    assert_eq!(outcome, Outcome::new(diags.clone()));

    // `lib` asked without `core`: no check key for its dependency, so a miss.
    let alone = fresh(&mut second, &[(lib2, inputs)], &store);
    assert!(!alone.has(lib2) && alone.keys.is_empty());

    // Served a crate at a time: `lib` asked after `core` is keyed by what `core` was found under,
    // and `core` keeps what it had.
    let mut stepped = Served::default();
    serve(&mut second, &[(core2, inputs)], &store, &mut stepped).unwrap();
    assert!(stepped.has(core2) && !stepped.has(lib2));
    serve(&mut second, &[(lib2, inputs)], &store, &mut stepped).unwrap();
    assert!(stepped.has(core2) && stepped.has(lib2));
    assert_eq!(stepped.keys, checks.keys);
    // Asked again under the same keys: the same, without a lookup.
    serve(&mut second, &[(lib2, inputs), (core2, inputs)], &store, &mut stepped).unwrap();
    assert_eq!(stepped.keys, checks.keys);
    assert_eq!(read(&second, &stepped, lib2, root).unwrap().unwrap(), Outcome::new(diags.clone()));

    // Other lint flags: the same check keys and bodies, another outcome key, the crate checked.
    let other = self::flags(&["--cap-lints", "warn"]);
    let other = self::inputs(&other);
    let missed = fresh(&mut second, &[(lib2, other), (core2, other)], &store);
    assert_eq!(missed.keys[&lib2], kept.keys[&lib]);
    assert!(!missed.has(lib2) && read(&second, &missed, lib2, root).is_none());
    // The same crates asked again under the other flags lose their outcomes, keep their bodies.
    serve(&mut second, &[(lib2, other), (core2, other)], &store, &mut stepped).unwrap();
    assert!(!stepped.has(lib2) && !stepped.has(core2));
    assert_eq!(stepped.keys, checks.keys);

    // A crate read from no image has no check key.
    let main = crate_named(&second, "main");
    assert_eq!(image_key(&second, main), None);
    assert!(fresh(&mut second, &[(main, inputs)], &store).keys.is_empty());
}

/// `lib`'s body reads a process variable its image's key cannot name.
const READING: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
fn main() { lib::v(); }

//- /lib.rs crate:lib deps:core new_source_root:library env:CARGO_MANIFEST_DIR=/reg/lib-1.0.0
pub fn v() -> u32 { let x: u32 = core::env!("BLAZE_CHECK_CACHE_TEST"); x }

//- /core.rs crate:core new_source_root:library
#[lang = "sized"]
pub trait Sized {}
#[rustc_builtin_macro]
#[macro_export]
macro_rules! env {}
"#;

/// What a body's `env!` reads files its check key: served while the variable is as it was,
/// missed once it is another.
#[test]
fn a_bodys_process_variable_files_the_outcome_by_its_value() {
    let store = Memory::default();
    let flags = flags(&[]);
    let inputs = inputs(&flags);
    let root = "/home/a/work";
    let var = [("BLAZE_CHECK_CACHE_TEST", "1")];
    let (mut db, _, diags) = checked_with(READING, "/reg/lib-1.0.0", root, &var);
    assert!(!diags.is_empty(), "the body's mismatch");
    let (core_image, lib_image) = imaged(&mut db, &store);
    let checked = [
        Checked { krate: crate_named(&db, "core"), image: core_image, inputs, diags: &[] },
        Checked { krate: crate_named(&db, "lib"), image: lib_image, inputs, diags: &diags },
    ];
    let kept = super::store(&db, &checked, &Served::default(), root, &store).unwrap();
    assert!(kept.refused.is_empty(), "{:?}", kept.refused);
    let served_with = |value: &str| {
        let fx = fixture::db(READING);
        let _ = fx.files().to_vec();
        let mut db = fx.into_db();
        base_db::set_process_env(&mut db, Env::from_iter([("BLAZE_CHECK_CACHE_TEST".into(), value.into())]));
        let all = base_db::all_crates(&db).to_vec();
        deps::serve(&mut db, &all, BUILD, &store, deps::Mode::Serve).unwrap();
        let (core, lib) = (crate_named(&db, "core"), crate_named(&db, "lib"));
        fresh(&mut db, &[(core, inputs), (lib, inputs)], &store).has(lib)
    };
    assert!(served_with("1"));
    assert!(!served_with("2"));
}

#[test]
fn a_bodies_part_under_its_key_that_does_not_open_is_an_error() {
    let store = Memory::default();
    let flags = flags(&[]);
    let inputs = inputs(&flags);
    let fx = fixture::db(FIXTURE);
    let _ = fx.files().to_vec();
    let mut first = fx.into_db();
    let (core_image, lib_image) = imaged(&mut first, &store);
    let core = [Checked { krate: crate_named(&first, "core"), image: core_image, inputs, diags: &[] }];
    let kept = super::store(&first, &core, &Served::default(), "/w", &store).unwrap();
    let core_key = kept.keys[&core[0].krate];
    // `lib` reads nothing, so its part is under its key itself.
    store.put(bodies_key(lib_image, &[core_key], inputs.borrowck), b"not a part".to_vec()).unwrap();
    let mut db = served_db(FIXTURE, &[], &store);
    let asked = [(crate_named(&db, "core"), inputs), (crate_named(&db, "lib"), inputs)];
    let error = serve(&mut db, &asked, &store, &mut Served::default()).err().unwrap();
    assert!(error.contains("crate lib") && error.contains("do not read"), "{error}");
}

/// What [`serve`] finds of `crates` asked first.
fn fresh(db: &mut Db, crates: &[(Crate, Inputs<'_>)], store: &Memory) -> Served {
    let mut served = Served::default();
    serve(db, crates, store, &mut served).unwrap();
    served
}

/// [`header`] before it read with `blaze_format::cursor`.
fn header_reference(bytes: &[u8]) -> Result<(bool, u32), &'static str> {
    let Some((head, body)) = bytes.split_first_chunk::<HEADER>() else {
        return Err("shorter than its header");
    };
    let word = |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
    if head[..4] != MAGIC {
        return Err("not an outcome of this encoding");
    }
    let flags = word(4);
    if flags & !FAILED != 0 {
        return Err("flags it does not know");
    }
    let (count, len) = (word(8), word(12));
    if len as usize != body.len() {
        return Err("a body whose length is not its header's");
    }
    if count > len {
        return Err("more diagnostics than bytes");
    }
    Ok((flags & FAILED != 0, count))
}

#[test]
fn the_header_reads_as_it_did_before_the_cursor() {
    let paths = || Paths::new(Some("/reg/lib".into()), Some("/out".into()));
    let diags = vec![diag_in("/reg/lib/src/lib.rs", "/out"), Diag::lint("dead_code", "unused", span("/reg/lib/a.rs", 1, 2))];
    let bytes = encode(&diags, &paths(), "/w/root/a").unwrap();
    let empty = encode(&[], &paths(), "/w").unwrap();
    assert_eq!(&bytes[..HEADER], &header_bytes(&bytes)[..]);
    for bytes in [&bytes, &empty] {
        assert_eq!(header(bytes), header_reference(bytes));
        for len in 0..bytes.len() {
            assert_eq!(header(&bytes[..len]), header_reference(&bytes[..len]), "cut at {len}");
        }
        for at in 0..HEADER {
            for bit in 0..8 {
                let mut damaged = bytes.clone();
                damaged[at] ^= 1 << bit;
                assert_eq!(header(&damaged), header_reference(&damaged), "byte {at} bit {bit}");
            }
        }
    }
}

/// The header [`encode`] wrote before it wrote with `blaze_format::cursor::Put`.
fn header_bytes(bytes: &[u8]) -> Vec<u8> {
    let (failed, count) = header_reference(bytes).unwrap();
    let flags = if failed { FAILED } else { 0 };
    let len = u32::try_from(bytes.len() - HEADER).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out
}

#[hegel::test]
fn any_bytes_read_as_before_the_cursor(tc: hegel::TestCase) {
    let mut bytes = tc.draw(hegel::generators::binary().max_size(23));
    let magic = tc.draw(hegel::generators::booleans());
    if magic && bytes.len() >= 4 {
        bytes[..4].copy_from_slice(&MAGIC);
    }
    assert_eq!(header(&bytes), header_reference(&bytes));
}

/// The copy of `absolute` this file had before it called hir-def's.
fn absolute_reference(path: &str) -> bool {
    match path.as_bytes() {
        [b'/', ..] | [b'\\', b'\\', ..] => true,
        [drive, b':', b'\\' | b'/', ..] => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

#[test]
fn a_path_is_absolute_as_the_old_copy_said() {
    let paths = [
        "", "/", "/home/a/src/lib.rs", "\\\\server\\share", "\\", "C:\\a", "c:/a", "C:", "C:a", "1:/a", "é:/a",
        "src/lib.rs", "./a", "$DIR/a.rs", "~/a",
    ];
    for path in paths {
        assert_eq!(hir_def::deps::absolute(path), absolute_reference(path), "{path:?}");
    }
}

#[hegel::test]
fn any_path_is_absolute_as_the_old_copy_said(tc: hegel::TestCase) {
    let path = tc.draw(hegel::generators::from_regex("[/\\\\a-zA-Z0-9:._~é$]{0,6}"));
    assert_eq!(hir_def::deps::absolute(&path), absolute_reference(&path));
}
