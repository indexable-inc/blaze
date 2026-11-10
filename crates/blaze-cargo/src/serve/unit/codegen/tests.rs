#![forbid(unsafe_code)]

use std::collections::HashMap;

use blaze_codegen::{plan::OptLevel, settings::Sanitize};
use blaze_units::schema::{DebugInfo, Profile, Strip, Stripped};

use blaze_codegen::{Init, Relocs, Static};
use blaze_ids::ContentId;
use blaze_items::ItemKey;

use super::{
    Full, KeptImage, Laid, LinkCause, Linked, Linking, Mounted, OutputFields, PgoFlag, StaticRow,
    codegen_backend, full_link_note, function_rows, kept_image, linking, pgo, read_profile,
    sanitize, static_rows,
};
use blaze_codegen::InstanceKey;

const HOST: &str = "x86_64-unknown-linux-gnu";

#[test]
fn a_crate_layout_override_is_refused_before_codegen_can_reuse_another_layouts_key() {
    let target = blaze_lower::target_data(HOST).unwrap();
    let mut workspace = base_db::CrateWorkspaceData {
        target: Ok(target), toolchain: None, const_eval: Default::default(),
    };
    assert_eq!(super::verify_target_data(HOST, &workspace), Ok(()));
    let original = workspace.target.clone(); // clone: restore the layout after each control
    workspace.target.as_mut().unwrap().data_layout = "e-p:32:32".into();
    let error = super::verify_target_data(HOST, &workspace).unwrap_err();
    assert!(error.contains(HOST) && error.contains("e-p:32:32") && error.contains("toolchain key"), "{error}");
    workspace.target = original;
    assert_eq!(super::verify_target_data(HOST, &workspace), Ok(()));
    workspace.target.as_mut().unwrap().has_thumb_interworking = true;
    assert!(super::verify_target_data(HOST, &workspace).is_err(), "the target's other layout facts count too");
    workspace.target = Err("the crate's target could not be loaded".into());
    assert_eq!(super::verify_target_data(HOST, &workspace).unwrap_err(), "the crate's target could not be loaded");
}

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
        contract_checks: true,
    }
}

#[test]
fn llvm_and_no_backend_leave_the_choice_to_blaze() {
    for level in [OptLevel::O0, OptLevel::O3] {
        assert_eq!(codegen_backend(&profile("dev", "0", None), "app", level), Ok(false));
        assert_eq!(codegen_backend(&profile("dev", "0", Some("llvm")), "app", level), Ok(false));
    }
}

#[test]
fn cranelift_is_taken_where_the_level_compiles_with_stencils() {
    let dev = profile("dev", "0", Some("cranelift"));
    assert_eq!(codegen_backend(&dev, "app", OptLevel::O0), Ok(false));
    assert_eq!(codegen_backend(&dev, "app", OptLevel::O1), Ok(false));
    let release = profile("release", "3", Some("cranelift"));
    let why = codegen_backend(&release, "app", OptLevel::O3).unwrap_err();
    assert!(
        why.contains("`release` of `app`")
            && why.contains("never Cranelift's")
            && why.contains("`stencil`"),
        "{why}"
    );
}

/// The seed's builds: `stencil` asks for stencils at every level, LLVM's too.
#[test]
fn stencil_asks_for_stencils_at_every_level() {
    assert_eq!(
        codegen_backend(&profile("dev", "0", Some("stencil")), "app", OptLevel::O0),
        Ok(true)
    );
    assert_eq!(
        codegen_backend(&profile("release", "3", Some("stencil")), "app", OptLevel::O3),
        Ok(true)
    );
}

#[test]
fn a_backend_blaze_has_no_setting_for_is_an_error_naming_it() {
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
    assert_eq!(
        sanitize(&words(&["-Ctarget-cpu=native", "-Zthreads=8"]), HOST),
        Ok(Sanitize::default())
    );
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

#[cfg(feature = "native")]
#[test]
fn a_profile_read_is_named_by_its_content_and_given() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = b"an indexed profile's bytes".to_vec();
    std::fs::write(dir.path().join("merged.profdata"), &bytes).unwrap();
    let read = read_profile(dir.path().to_str().unwrap(), "merged.profdata")
        .unwrap_or_else(|what| panic!("{what}"));
    assert_eq!(read.id, hir_ty_codegen::v2::profile_id(&bytes));
    assert_eq!(read._given.id(), read.id);
    assert_eq!(read.unstored.lock().as_deref(), Some(&bytes[..]));
}

#[cfg(not(feature = "native"))]
#[test]
fn a_profile_to_use_without_llvm_stops_naming_the_feature() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("merged.profdata"), b"profile").unwrap();
    let refused = read_profile(dir.path().to_str().unwrap(), "merged.profdata").err().unwrap();
    assert!(
        refused.contains("`-C profile-use=merged.profdata`") && refused.contains("`llvm`"),
        "{refused}"
    );
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

    let (next, gone, new) =
        static_rows(&first, &[a_static(2, 0xbb), a_static(1, 0xcc), a_static(3, 0)]).unwrap();
    assert_eq!(items(&next), [1, 2, 3]);
    assert_eq!(items(&gone), [1], "the old value of the static whose bytes changed");
    assert_eq!(items(&new), [1, 3]);
}

/// Function `n` (its key `[n; 32]`), calling each of `callees` through its slot.
fn calling(n: u8, callees: &[u8]) -> blaze_codegen::CompiledFunction {
    let key = |n: u8| InstanceKey(ContentId([n; 32]));
    let relocs: Vec<blaze_codegen::Relocation> = callees
        .iter()
        .enumerate()
        .map(|(at, callee)| blaze_codegen::Relocation {
            at: at as u32 * 5 + 1,
            kind: blaze_codegen::RelocKind::PcRel32,
            target: blaze_codegen::Target::Function(key(*callee)),
            addend: -4,
        })
        .collect();
    let mut code: Vec<u8> = callees.iter().flat_map(|_| [0xe8, 0, 0, 0, 0]).collect();
    code.push(0xc3);
    blaze_codegen::CompiledFunction {
        key: key(n),
        arch: blaze_codegen::Arch::X86_64,
        align: 16,
        code,
        relocs: relocs.into(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    }
}

/// `log` (key 1) is edited, which gives it a new key (4). `call` (key 2), which calls it, keeps
/// its key, as a function's key does not say which callee is at each of its places
/// (`driver::placed`), and the build fills its relocation with key 4. The relink must take
/// `call` again: its old piece still calls key 1, which the change removes ("the function ..
/// is removed, and .. still refers to it"). `main` (key 3), which calls `call` and whose fill
/// is unchanged, is in neither list. Two fills of one key in one build are refused, naming
/// the function.
#[test]
fn an_edit_to_a_callee_relinks_its_unchanged_callers() {
    let key = |n: u8| InstanceKey(ContentId([n; 32]));
    let names = [(key(2), "call".to_string())];
    let (first, gone, new) =
        function_rows(&[], &[calling(3, &[2]), calling(2, &[1]), calling(1, &[])], &names).unwrap();
    assert!(gone.is_empty());
    assert_eq!(new, [key(1), key(2), key(3)]);

    let (same, gone, new) =
        function_rows(&first, &[calling(1, &[]), calling(2, &[1]), calling(3, &[2])], &names)
            .unwrap();
    assert_eq!(same, first);
    assert!(gone.is_empty() && new.is_empty(), "linked again unchanged: {gone:?} {new:?}");

    let (_, gone, new) =
        function_rows(&first, &[calling(3, &[2]), calling(2, &[4]), calling(4, &[])], &names)
            .unwrap();
    assert_eq!(gone, [key(1), key(2)], "the old `log`, and `call` as it was filled against it");
    assert_eq!(new, [key(2), key(4)], "`call` filled against the new `log`, and the new `log`");

    let twice = function_rows(&[], &[calling(2, &[1]), calling(2, &[4])], &names).unwrap_err();
    assert!(
        twice.contains("filled two ways")
            && twice.contains("`call`")
            && twice.contains(&format!("{:?}", key(2))),
        "{twice}"
    );
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
        link: blaze_link::LinkArgs::default(),
    }
}

fn kept(path: &str, output: &blaze_link::Output) -> KeptImage {
    let to = blaze_core::FileName::new(path.trim_start_matches('/')).unwrap();
    let kept = Linked {
        image: blaze_link::Image::new(to, output.clone()).unwrap(),
        output: output.clone(),
        functions: vec![(InstanceKey(ContentId([9; 32])), ContentId([9; 32]))],
        statics: Vec::new(),
        natives: Vec::new(),
        names: Vec::new(),
        layout: None,
        loaded: None,
        slotted: Vec::new(),
        assumed: Vec::new(),
        given: None,
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
    let new =
        blaze_link::Output { debug: blaze_link::Debug::Lines, exec_log: true, ..shared(&["g"]) };
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
    assert_eq!(linking(None, Laid::Whole, true), Linking::First);
    assert_eq!(linking(Some(OutputFields::default()), Laid::Patched, true), Linking::Spliced);
    assert_eq!(linking(Some(OutputFields::default()), Laid::Nothing, true), Linking::Unchanged);
    assert_eq!(
        linking(Some(OutputFields::default()), Laid::Whole, true),
        Linking::First,
        "a kept image never laid out"
    );
    assert_eq!(
        linking(Some(OutputFields::default()), Laid::Whole, false),
        Linking::Full(Full::NotIncremental)
    );
    assert_eq!(linking(Some(exports), Laid::Whole, true), Linking::Full(Full::Output(exports)));
    assert_eq!(
        Linking::Full(Full::Output(exports)).to_string(),
        "not spliced: the output changed: exports"
    );
    assert!(Linking::First.placed() && !Linking::Spliced.placed() && !Linking::Unchanged.placed());
    // A splice refused for a thunk: the link from scratch is said on the link line, and the
    // build names the branch.
    assert!(Linking::Full(Full::NoThunk).placed());
    // A splice refused for want of room: linked from scratch, and said so.
    assert!(Linking::Full(Full::NoRoom).placed());
    assert_eq!(
        Linking::Full(Full::NoRoom).to_string(),
        "not spliced: full link: the kept image has no room for the change"
    );
    assert_eq!(
        Linking::Full(Full::NoThunk).to_string(),
        "not spliced: full link: the splice could not place a thunk in a branch's reach"
    );
    assert_eq!(
        full_link_note("A64Call26 at 0x114 in the function `f`"),
        "full link: the splice could not place a thunk for A64Call26 at 0x114 in the function `f`"
    );
}

/// The image kept at a path is the one the link stages against, taken out of the kept images;
/// one of another output, or laid out by another profile, is let go only once its replacement
/// is made.
#[test]
fn a_kept_image_is_taken_only_for_its_own_output_and_layout() {
    let (path, output) = ("/w/target/debug/libm.so", shared(&["f"]));
    let mut images = vec![kept(path, &output)];
    let (linked, changed, relaid) = kept_image(&mut images, path, &output, None).unwrap();
    assert_eq!(
        (linked.functions.len(), changed, relaid),
        (1, Some(OutputFields::default()), false)
    );
    assert!(images.is_empty(), "taken out for the link");

    images.push(KeptImage { path: path.to_owned(), kept: linked });
    let heat = Some(ContentId([3; 32]));
    let (fresh, changed, relaid) = kept_image(&mut images, path, &output, heat).unwrap();
    assert!(
        fresh.functions.is_empty() && fresh.layout == heat && relaid,
        "another profile: laid out again"
    );
    assert_eq!(changed, Some(OutputFields::default()));
    assert!(images.is_empty(), "the image of the old layout is let go");

    images.push(KeptImage { path: path.to_owned(), kept: fresh });
    let other = shared(&["f", "g"]);
    let (fresh, changed, _) = kept_image(&mut images, path, &other, heat).unwrap();
    assert!(fresh.functions.is_empty() && fresh.output == other);
    assert_eq!(changed.map(|it| it.to_string()).as_deref(), Some("exports"));
    assert!(images.is_empty(), "the image of the old output is let go");

    let (fresh, changed, relaid) = kept_image(&mut images, path, &output, None).unwrap();
    assert!(
        fresh.functions.is_empty() && changed.is_none() && !relaid,
        "nothing kept: a first link"
    );

    let unnamed = "/w/../libm.so";
    let mut images = vec![
        kept("/w/target/debug/libm.so", &output),
        KeptImage { path: unnamed.into(), ..kept(path, &output) },
    ];
    let error =
        kept_image(&mut images, unnamed, &other, None).err().expect("a path the linker can't name");
    assert!(matches!(error, LinkCause::Unnamed), "{error}");
    assert_eq!(
        images.len(),
        2,
        "a link that fails before staging leaves the kept images as they were"
    );
}

/// A link blaze-link refused for want of room, which a link from scratch could not take
/// either (a kept image out of room is linked from scratch first, `Full::NoRoom`), names the
/// image, what overflowed and that the limit is the output's; any other refusal says no such
/// thing.
#[test]
fn a_link_out_of_room_names_the_image_and_whose_limit_it_is() {
    let refused = |refusal| blaze_link::LinkError {
        function: None,
        what: "Patch needs 70000000 bytes and has room for 67108864: link the image from scratch"
            .to_owned(),
        refusal,
    };
    let failed = |refusal| {
        super::LinkFailed {
            image: "/w/target/debug/app".to_owned(),
            cause: LinkCause::Linker { step: super::LinkStep::Stage, error: refused(refusal) },
        }
        .to_string()
    };
    let no_room = failed(Some(blaze_link::Refusal::NoRoom { need: 70_000_000, room: 67_108_864 }));
    assert!(
        no_room.starts_with("linking `/w/target/debug/app`: staging the change: "),
        "{no_room}"
    );
    assert!(no_room.contains("Patch needs 70000000 bytes and has room for 67108864"), "{no_room}");
    assert!(no_room.contains("the limit is the output's"), "{no_room}");
    assert!(!failed(None).contains("the limit is the output's"));
}
