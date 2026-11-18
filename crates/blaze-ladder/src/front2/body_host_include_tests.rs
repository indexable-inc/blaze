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
use super::source::{Load, Log, NamesSource, Records};
use super::tables::recorder::{Recorder, RecorderOpts};
use super::tables::view::{Tables, TablesOpts};

const LIB: &str = r#"#![no_core]
pub fn f() { let _ = (include_str!("../assets/text"), include_bytes!("blob")); }
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

    fn expansion(&self, _: usize, _: ItemKey) -> Option<&'a str> {
        None
    }

    fn expansion_origins(&self, _: usize, _: ItemKey) -> Option<(&'a [u8], &'a [u8])> {
        None
    }
}

#[test]
fn a_record_backed_body_reads_data_relative_to_its_calling_file() {
    let frame = Frame::new(blaze_mem::tag::Tag::Infer).expect("the records frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let files = Files {
        env: CrateEnv::of(EnvFacts {
            crate_name: "pkg", package: Some(("pkg", "1.0.0")), bin: None,
            seen: SeenRoot::Package, read_folder: "/7/pkg/", build_script: false,
            script: None, test_or_bench: false, meta: None,
        }, alloc).expect("the reader's package"),
        reads: Cell::new(0),
    };
    let input = CrateInput {
        name: "pkg", key: CrateItemKey(ContentId::of("record-backed body include test", b"pkg")),
        root: "/old/pkg/src/lib.rs", shown_from: "/old/pkg/", edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        features: &[], deps: &[], cfg: &[], script: None,
    };
    let (kept, item, module) = {
        let solved = resolve_crates(&[input], Sysroot::default(), &Original, &Serial, alloc).expect("resolve the fixture");
        let (item, module, _) = solved.items(0)
            .find(|(key, _, _)| solved.item_text(*key).and_then(|row| row.name) == Some("f"))
            .expect("the function f");
        let krate = solved.krate(0);
        let kept = Kept {
            names: aligned(&names_record::encode(krate, alloc).expect("names record"), alloc),
            items: aligned(&items_record::encode(krate, alloc).expect("items record"), alloc),
        };
        (kept, item, module)
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

    let planted = BodyHost::planted_solved_file(tables, &sources, 0, item, module, alloc);
    assert_eq!(planted.include("../assets/text", IncludeAs::Str), Err(HostError { ask: HostAsk::Include, why: HostWhy::Refused }));
    assert_eq!(files.reads.get(), 3, "the solved-only lookup refused before reading the file");

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
