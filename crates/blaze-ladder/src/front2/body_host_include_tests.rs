use core::cell::Cell;

use blaze_expand::seen::SeenRoot;
use blaze_expand::{DefAt, DefSite, ExpandCx, Export, HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner, ReadAnswer, ReadKind};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_mem::Frame;
use blaze_scope::crate_env::{CrateEnv, EnvFacts};
use blaze_scope::graph::Sysroot;
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{CrateInput, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::body_host::{BodyHost, Sourced};
use super::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use super::tables::recorder::{Recorder, RecorderOpts};
use super::tables::view::{Tables, TablesOpts};

const LIB: &str = r#"#![no_core]
pub fn f() { let _ = (include_str!("../assets/text"), include_bytes!("blob")); }
macro_rules! make { () => { fn generated() { let _ = include_str!("../assets/text"); } }; }
make!();
"#;
const TEXT: &[u8] = "café\n".as_bytes();
const BLOB: &[u8] = &[0xff, 0, 0xfe];

struct Original;

impl<'a> SourceTexts<'a> for Original {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        (krate == 0 && path == "/old/pkg/src/lib.rs").then_some(LIB)
    }
}

struct Files<'a> {
    env: CrateEnv<'a>,
    reads: Cell<usize>,
}

impl<'a> SourceTexts<'a> for Files<'a> {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        panic!("body data read as Rust text in crate {krate} at {path}")
    }

    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'a>> {
        (krate == 0).then_some(&self.env)
    }

    fn read_data(&self, krate: usize, from: &str, path: &str) -> Option<&'a [u8]> {
        assert_eq!((krate, from), (0, "/7/pkg/src/lib.rs"));
        self.reads.set(self.reads.get() + 1);
        match path {
            "/7/pkg/assets/text" => Some(TEXT),
            "/7/pkg/src/blob" => Some(BLOB),
            "/7/pkg/src/missing" => None,
            _ => panic!("unexpected data read from {from} of {path}"),
        }
    }
}

struct Kept<'a> {
    names: &'a [u8],
    items: &'a [u8],
    /// The `make!()` call's expansion as its record keeps it: the call, its text, its origins
    /// in their joined form (empty: the solve kept none).
    made: (ItemKey, &'a str, &'a [u8]),
}

impl<'a> Load<'a> for Kept<'a> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> {
        (krate == 0).then_some(self.names)
    }

    fn items(&self, krate: usize) -> Option<&'a [u8]> {
        (krate == 0).then_some(self.items)
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str> {
        (krate == 0 && path.as_str() == "src/lib.rs").then_some(LIB)
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'a>> {
        let (made, text, bytes) = self.made;
        if krate != 0 || call != made {
            return None;
        }
        let origins = if bytes.is_empty() {
            (&[][..], &[][..])
        } else {
            blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e:?}"))
        };
        Some(LoadedExpansion { text, origins: Some(origins) })
    }
}

/// Before (tempo c0255wave87, 8 `#[tokio::test]` bodies of crates/node/tests/it): over records
/// a macro-made body's include was `Refused` (body_host.rs `body_file` walked `place_of` of the
/// expansion's call, which is no item row). Now `BodyHost::of` reads the call part of the
/// expansion's origins (`NamesSource::call_text`) and the include is relative to the calling
/// file (the Reference, `include!`). Twin: `../../elsewhere` from the same body is `Outside`.
/// Run t67 (2026-11-21) still failed `Refused` here: the body host is given the `Tables` view,
/// which kept `NamesSource::call_text`'s default (`call_origin`, refused over records by
/// design); tables/view.rs now serves it from the records' call part. Twin: `missing` from
/// the macro-made body is `Missing`, asked relative to the calling file.
#[test]
fn a_record_backed_body_reads_data_relative_to_its_calling_file() {
    let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("the records frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let files = Files {
        env: CrateEnv::of(EnvFacts {
            crate_name: "pkg", package: Some(("pkg", "1.0.0")), bin: None,
            seen: SeenRoot::Package, read_folder: "/7/pkg/", build_script: false,
            script: None, test_or_bench: false, bin_exes: blaze_scope::crate_env::BinNames::NONE, meta: None,
        }, alloc).expect("the reader's package"),
        reads: Cell::new(0),
    };
    let input = CrateInput {
        name: "pkg", key: CrateItemKey(ContentId::of("record-backed body include test", b"pkg")),
        root: "/old/pkg/src/lib.rs", shown_from: "/old/pkg/", edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        features: &[], deps: &[], cfg: &[], script: None,
    };
    let (kept, item, module, generated) = {
        let solved = resolve_crates(&[input], Sysroot::default(), &Original, &Serial, bump).expect("resolve the fixture");
        let (item, module, _) = solved.items(0)
            .find(|(key, _, _)| solved.item_text(*key).and_then(|row| row.name) == Some("f"))
            .expect("the function f");
        let generated = solved.items(0)
            .find(|(key, _, _)| solved.item_text(*key).and_then(|row| row.name) == Some("generated"))
            .map(|(key, _, _)| key)
            .expect("the macro-made function");
        let sourced = Sourced(&files);
        {
            let solved_host = BodyHost::of(&solved, Some(&sourced), 0, generated, module, alloc);
            assert_eq!(solved_host.include("../assets/text", IncludeAs::Str), Ok(Included::Bytes(TEXT)));
        }
        files.reads.set(0);
        let krate = solved.krate(0);
        // The `make!()` call `generated` is written by, and its expansion as the solve made it.
        let Some(blaze_scope::resolve::At::Expansion(call)) = NamesSource::place_of(&solved, generated).map(|place| place.at) else {
            panic!("the macro-made function is placed in an expansion")
        };
        let made = krate.expansions.get(&call).and_then(|e| e.made).expect("the `make!()` call's text");
        let origins: &[u8] = match made.origins {
            Ok(bytes) => bytes,
            Err(e) => panic!("the origins of the `make!()` call: {e:?}"),
        };
        let kept = Kept {
            names: aligned(&names_record::encode(krate, alloc).expect("names record"), bump),
            items: aligned(&items_record::encode(krate, alloc).expect("items record"), bump),
            made: (call, made.text, origins),
        };
        (kept, item, module, generated)
    };
    let sources = Sourced(&files);
    let keys = [input.key];
    // In the frame, so it lives as long as the records made in it (first compile: a local
    // declared after the frame did not).
    let log = &*frame.alloc(Log::new(&["pkg"], alloc));
    let records = frame.alloc(Records::new(&keys, &kept, log, &frame, alloc));
    let recorder = Recorder::new(RecorderOpts { alloc });
    let tables = Tables::new(TablesOpts { records, reads: &recorder, impls: None, lang_crates: &[] });
    let host = BodyHost::of(tables, Some(&sources), 0, item, module, alloc);
    assert_eq!(host.include("../assets/text", IncludeAs::Str), Ok(Included::Bytes(TEXT)));
    assert_eq!(host.include("blob", IncludeAs::Bytes), Ok(Included::Bytes(BLOB)));
    assert_eq!(host.include("missing", IncludeAs::Str), Ok(Included::Missing));
    assert_eq!(host.include("../../elsewhere", IncludeAs::Bytes), Ok(Included::Outside));
    assert_eq!(files.reads.get(), 3, "the missing file was consulted and the outside file was not");

    // The macro-made body over records: its include is relative to the file the `make!()` call
    // is written in, read from the call part of the expansion's origins (`call_text`).
    let generated_records = BodyHost::of(tables, Some(&sources), 0, generated, module, alloc);
    assert_eq!(generated_records.include("../assets/text", IncludeAs::Str), Ok(Included::Bytes(TEXT)), "a macro-made body over records");
    assert_eq!(generated_records.include("../../elsewhere", IncludeAs::Bytes), Ok(Included::Outside), "TWIN: a path out of the package from the macro-made body");
    assert_eq!(files.reads.get(), 4, "the macro-made body's include read once, the outside one never");
    // TWIN: a path with no file, from the macro-made body: asked of the reader as
    // `/7/pkg/src/missing` from `/7/pkg/src/lib.rs` (`Files::read_data` asserts the calling file
    // and panics on any other path), answered `Missing`, never `Refused`: the file asked is the
    // written path joined to the calling file's folder.
    assert_eq!(generated_records.include("missing", IncludeAs::Str), Ok(Included::Missing), "TWIN: a missing file from the macro-made body");
    assert_eq!(files.reads.get(), 5, "the missing file was asked once, relative to the calling file");
    files.reads.set(4);

    let source = records.file_paths(0, 0).expect("the invocation's physical file");
    // The body stage's physical invocation file, given: the same answer.
    let generated_host = BodyHost::of_with_file(tables, Some(&sources), 0, generated, module, source, alloc);
    assert_eq!(generated_host.include("../assets/text", IncludeAs::Str), Ok(Included::Bytes(TEXT)));
    assert_eq!(files.reads.get(), 5, "the macro-made body's include is resolved against its invocation file");

    let planted = BodyHost::planted_solved_file(tables, &sources, 0, item, module, alloc);
    assert_eq!(planted.include("../assets/text", IncludeAs::Str), Err(HostError { ask: HostAsk::Include, why: HostWhy::Refused }));
    assert_eq!(files.reads.get(), 5, "the solved-only lookup refused before reading the file");

    let builtin = DefAt {
        site: DefSite { krate: input.key, module: Some(module) },
        edition: input.edition, export: Export::Exported, local_inner: LocalInner::No, transparency: None,
    };
    let mut expand = blaze_lower::by_name::ByName::new(&[], builtin, host, alloc);
    let text = tables.text_of(item).expect("the recorded function body").text;
    let lowered = blaze_lower::lower_body_expanding(text, input.edition, &mut super::heads::CrateCfg(&[]), &mut blaze_lower::NoModule, &mut expand, alloc).expect("lower the two builtins");
    assert_eq!(lowered.view().unhandled().count(), 0);
    assert!(expand.findings().is_empty());
    let reads = expand.reads();
    assert_eq!(reads.list().len(), 2);
    for (kind, path, bytes) in [(ReadKind::IncludeStr, "../assets/text", TEXT), (ReadKind::IncludeBytes, "blob", BLOB)] {
        let read = reads.list().iter().find(|read| read.kind == kind && reads.subject(read) == path).expect(path);
        assert_eq!(read.answer, ReadAnswer::Digest(ContentId::of("blaze v2 expand included file 1", bytes)));
    }
    assert!(records.errors().is_empty(), "{:?}", records.errors());
}
