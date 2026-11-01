#![forbid(unsafe_code)]

use std::collections::HashMap;

use blaze_codegen::{plan::OptLevel, settings::Sanitize};
use blaze_units::schema::{DebugInfo, Profile, Strip, Stripped};

use blaze_codegen::{Init, Relocs, Static};
use blaze_ids::ContentId;
use blaze_items::ItemKey;

use super::{
    Full, KeptImage, LinkCause, Linked, Linking, Mounted, OutputFields, PgoFlag, StaticRow, codegen_backend,
    kept_image, linking, pgo, read_profile, sanitize, static_rows,
};
use blaze_codegen::InstanceKey;

const HOST: &str = "x86_64-unknown-linux-gnu";

fn profile(name: &str, opt_level: &str, backend: Option<&str>) -> Profile {
    Profile {
        name: name.to_owned(),
        opt_level: opt_level.to_owned(),
        lto: "false".to_owned(),
        codegen_backend: backend.map(str::to_owned),
        codegen_units: None,
        debuginfo: DebugInfo::Level(2),
        split_debuginfo: None,
        debug_assertions: true,
        overflow_checks: true,
        rpath: false,
        incremental: true,
        panic: "unwind".to_owned(),
        strip: Strip::Deferred(Stripped::None),
        rustflags: Vec::new(),
        trim_paths: None,
        hint_mostly_unused: None,
    }
}

#[test]
fn llvm_and_no_backend_leave_the_choice_to_blaze() {
    for level in [OptLevel::O0, OptLevel::O3] {
        assert_eq!(codegen_backend(&profile("dev", "0", None), "app", level), Ok(()));
        assert_eq!(codegen_backend(&profile("dev", "0", Some("llvm")), "app", level), Ok(()));
    }
}

#[test]
fn cranelift_is_taken_where_the_level_compiles_with_it() {
    let dev = profile("dev", "0", Some("cranelift"));
    assert_eq!(codegen_backend(&dev, "app", OptLevel::O0), Ok(()));
    assert_eq!(codegen_backend(&dev, "app", OptLevel::O1), Ok(()));
    let release = profile("release", "3", Some("cranelift"));
    let why = codegen_backend(&release, "app", OptLevel::O3).unwrap_err();
    assert_eq!(
        why,
        "the profile `release` of `app` asks for codegen-backend `cranelift`, and blaze's code \
         generator takes no backend over the one opt-level 3 compiles with"
    );
}

#[test]
fn a_backend_blaze_has_no_setting_for_is_an_error_naming_it() {
    let stencil = codegen_backend(&profile("dev", "0", Some("stencil")), "app", OptLevel::O0).unwrap_err();
    assert!(stencil.contains("`dev` of `app`") && stencil.contains("`stencil`"), "{stencil}");
    let gcc = codegen_backend(&profile("dev", "0", Some("gcc")), "dep", OptLevel::O0).unwrap_err();
    assert_eq!(
        gcc,
        "the profile `dev` of `dep` asks for codegen-backend `gcc`, which blaze has none of \
         (`cranelift`, `stencil`, `llvm`)"
    );
}

#[test]
fn a_plain_build_is_not_sanitized() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    assert_eq!(sanitize(&words(&["-Ctarget-cpu=native", "-Zthreads=8"]), HOST), Ok(Sanitize::default()));
    // SanitizerCoverage without its level instruments nothing, so nothing is to link.
    assert_eq!(sanitize(&words(&["-Cpasses=sancov-module"]), HOST), Ok(Sanitize::default()));
}

#[test]
fn a_sanitized_build_stops_with_why() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    let refused = sanitize(&words(&["-Zsanitizer=address"]), HOST).unwrap_err();
    assert!(refused.starts_with("-Z sanitizer=address: "), "{refused}");
    let coverage = words(&["-Cpasses=sancov-module", "-Cllvm-args=-sanitizer-coverage-level=3"]);
    assert_eq!(sanitize(&coverage, HOST).unwrap_err(), crate::unstable::UNLINKED);
    let unknown = sanitize(&words(&["-Zno-such-flag"]), HOST).unwrap_err();
    assert!(unknown.starts_with("-Z no-such-flag: "), "{unknown}");
}

#[test]
fn a_profile_to_make_stops_with_the_flag_and_why() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    assert_eq!(pgo(&words(&["-Ctarget-cpu=native", "-C", "opt-level=3"])), Ok(PgoFlag::None));
    for (flags, named) in [
        (&["-Cprofile-generate=/tmp/pgo"][..], "`-C profile-generate=/tmp/pgo`"),
        (&["-C", "profile-generate"][..], "`-C profile-generate`"),
        (&["--codegen", "profile_generate=/tmp/pgo"][..], "`-C profile_generate=/tmp/pgo`"),
        // Asked beside a profile to use, it still stops.
        (&["-Cprofile-use=/p", "-Cprofile-generate"][..], "`-C profile-generate`"),
    ] {
        let refused = pgo(&words(flags)).unwrap_err();
        assert!(refused.contains(named), "{refused}");
        assert!(refused.contains("linker writes no profile sections"), "{refused}");
    }
}

#[test]
fn a_profile_to_use_is_its_path_in_every_form_of_the_flag() {
    let words = |words: &[&str]| words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    for flags in [
        &["-Cprofile-use=/tmp/merged.profdata"][..],
        &["-C", "profile-use=/tmp/merged.profdata"][..],
        &["--codegen=profile-use=/tmp/merged.profdata"][..],
        &["--codegen", "profile-use=/tmp/merged.profdata"][..],
        &["-C", "opt-level=3", "-Cprofile_use=/tmp/merged.profdata", "-Ctarget-cpu=native"][..],
        // The last one given is the one, as rustc takes a codegen option.
        &["-Cprofile-use=/tmp/old.profdata", "-C", "profile-use=/tmp/merged.profdata"][..],
    ] {
        let flags = words(flags);
        assert_eq!(pgo(&flags), Ok(PgoFlag::Use("/tmp/merged.profdata")), "{flags:?}");
    }
    for flags in [&["-Cprofile-use"][..], &["-C", "profile-use="][..]] {
        let refused = pgo(&words(flags)).unwrap_err();
        assert!(refused.contains("names no profile"), "{refused}");
    }
}

#[test]
fn a_profile_that_is_not_there_is_an_error_naming_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let missing = read_profile(root, "pgo/merged.profdata").err().unwrap();
    assert!(missing.contains("`-C profile-use=pgo/merged.profdata`"), "{missing}");
    let at = dir.path().join("pgo/merged.profdata");
    assert!(missing.contains(&format!("`{}` can't be read", at.display())), "{missing}");
    let absolute = read_profile(root, "/nonexistent/blaze/merged.profdata").err().unwrap();
    assert!(absolute.contains("`/nonexistent/blaze/merged.profdata` can't be read"), "{absolute}");
}

#[cfg(feature = "llvm")]
#[test]
fn a_profile_read_is_named_by_its_content_and_given() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = b"an indexed profile's bytes".to_vec();
    std::fs::write(dir.path().join("merged.profdata"), &bytes).unwrap();
    let read = read_profile(dir.path().to_str().unwrap(), "merged.profdata").unwrap_or_else(|what| panic!("{what}"));
    assert_eq!(read.id, hir_ty_codegen::v2::profile_id(&bytes));
    assert_eq!(read._given.id(), read.id);
    assert_eq!(read.unstored.lock().as_deref(), Some(&bytes[..]));
}

#[cfg(not(feature = "llvm"))]
#[test]
fn a_profile_to_use_without_llvm_stops_naming_the_feature() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("merged.profdata"), b"profile").unwrap();
    let refused = read_profile(dir.path().to_str().unwrap(), "merged.profdata").err().unwrap();
    assert!(refused.contains("`-C profile-use=merged.profdata`") && refused.contains("`llvm`"), "{refused}");
}

#[test]
fn what_names_a_file_for_every_worker_is_send_and_sync() {
    fn shared<T: Send + Sync>() {}
    shared::<Mounted>();
    shared::<std::sync::Arc<HashMap<base_db::FileId, String>>>();
}

fn a_static(n: u8, value: u8) -> Static {
    Static {
        item: ItemKey(ContentId([n; 32])),
        instance: ContentId([n + 100; 32]),
        align: 4,
        init: Init::Bytes { bytes: vec![value; 4], relocs: Relocs::new() },
        data: Vec::new(),
        mutable: true,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

/// blaze-link's change is a delta: a static put again as it was must be in neither list, or
/// the relink moves it and rewrites every function that reads it.
#[test]
fn a_static_linked_again_unchanged_is_neither_gone_nor_new() {
    let items = |rows: &[StaticRow]| rows.iter().map(|it| it.0.0.0[0]).collect::<Vec<_>>();
    let (first, gone, new) = static_rows(&[], &[a_static(2, 0xbb), a_static(1, 0xaa)]).unwrap();
    assert_eq!((items(&first), items(&gone), items(&new)), (vec![1, 2], vec![], vec![1, 2]));

    let (same, gone, new) = static_rows(&first, &[a_static(1, 0xaa), a_static(2, 0xbb)]).unwrap();
    assert_eq!(same, first);
    assert!(gone.is_empty() && new.is_empty(), "{gone:?} {new:?}");

    let (next, gone, new) = static_rows(&first, &[a_static(2, 0xbb), a_static(1, 0xcc), a_static(3, 0)]).unwrap();
    assert_eq!(items(&next), [1, 2, 3]);
    assert_eq!(items(&gone), [1], "the old value of the static whose bytes changed");
    assert_eq!(items(&new), [1, 3]);
}

fn shared(exports: &[&str]) -> blaze_link::Output {
    blaze_link::Output {
        format: blaze_link::Format::Elf,
        arch: blaze_codegen::Arch::X86_64,
        kind: blaze_link::OutputKind::Shared {
            name: "libm.so".into(),
            exports: exports.iter().map(|it| (*it).to_owned()).collect(),
        },
        interpreter: None,
        platform: None,
        incremental: true,
        debug: blaze_link::Debug::None,
        live: None,
        tags: 1 << 32,
        stack: None,
        windows_subsystem: false,
        dev_run: None,
        triple: HOST.into(),
        exec_log: false,
        sanitize: Sanitize::default(),
        rpaths: Vec::new(),
    }
}

fn kept(path: &str, output: &blaze_link::Output) -> KeptImage {
    let to = blaze_core::FileName::new(path.trim_start_matches('/')).unwrap();
    let kept = Linked {
        image: blaze_link::Image::new(to, output.clone()).unwrap(),
        output: output.clone(),
        functions: vec![InstanceKey(ContentId([9; 32]))],
        statics: Vec::new(),
        natives: Vec::new(),
        names: Vec::new(),
    };
    KeptImage { path: path.to_owned(), kept }
}

/// A shared library's exports are every name it defines: an edit that adds a function changes
/// them, and the link must say so rather than call the new image a first link.
#[test]
fn an_output_that_changed_names_the_fields_that_differ() {
    let old = shared(&["f"]);
    assert!(OutputFields::between(&old, &old).is_empty());
    let new = shared(&["f", "g"]);
    assert_eq!(OutputFields::between(&old, &new).to_string(), "exports");
    let new = blaze_link::Output { debug: blaze_link::Debug::Lines, exec_log: true, ..shared(&["g"]) };
    assert_eq!(OutputFields::between(&old, &new).to_string(), "exports, debug, exec_log");
    let renamed = blaze_link::Output {
        kind: blaze_link::OutputKind::Shared { name: "libn.so".into(), exports: vec!["f".into()] },
        ..shared(&[])
    };
    assert_eq!(OutputFields::between(&old, &renamed).to_string(), "kind");
}

#[test]
fn how_a_link_went_is_first_only_with_nothing_kept() {
    let exports = OutputFields::between(&shared(&["f"]), &shared(&[]));
    assert_eq!(linking(None, true, true), Linking::First);
    assert_eq!(linking(Some(OutputFields::default()), false, true), Linking::Spliced);
    assert_eq!(linking(Some(OutputFields::default()), true, true), Linking::First, "a kept image never laid out");
    assert_eq!(linking(Some(OutputFields::default()), true, false), Linking::Full(Full::NotIncremental));
    assert_eq!(linking(Some(exports), true, true), Linking::Full(Full::Output(exports)));
    assert_eq!(Linking::Full(Full::Output(exports)).to_string(), "not spliced: the output changed: exports");
    assert!(Linking::First.placed() && !Linking::Spliced.placed());
}

/// The image kept at a path is the one the link stages against, taken out of the kept images;
/// one of another output is let go only once its replacement is made.
#[test]
fn a_kept_image_is_taken_only_for_its_own_output() {
    let (path, output) = ("/w/target/debug/libm.so", shared(&["f"]));
    let mut images = vec![kept(path, &output)];
    let (linked, changed) = kept_image(&mut images, path, &output).unwrap();
    assert_eq!((linked.functions.len(), changed), (1, Some(OutputFields::default())));
    assert!(images.is_empty(), "taken out for the link");

    images.push(KeptImage { path: path.to_owned(), kept: linked });
    let other = shared(&["f", "g"]);
    let (fresh, changed) = kept_image(&mut images, path, &other).unwrap();
    assert!(fresh.functions.is_empty() && fresh.output == other);
    assert_eq!(changed.map(|it| it.to_string()).as_deref(), Some("exports"));
    assert!(images.is_empty(), "the image of the old output is let go");

    let (fresh, changed) = kept_image(&mut images, path, &output).unwrap();
    assert!(fresh.functions.is_empty() && changed.is_none(), "nothing kept: a first link");

    let unnamed = "/w/../libm.so";
    let mut images = vec![kept("/w/target/debug/libm.so", &output), KeptImage { path: unnamed.into(), ..kept(path, &output) }];
    let error = kept_image(&mut images, unnamed, &other).err().expect("a path the linker can't name");
    assert!(matches!(error, LinkCause::Unnamed), "{error}");
    assert_eq!(images.len(), 2, "a link that fails before staging leaves the kept images as they were");
}
