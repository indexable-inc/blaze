//! The missing-library reply is a completed pkg-config observation. libusb1-sys can use it
//! to select its vendored implementation and its engine record can then certify a no-op.

use super::*;

mod executor;

use core::sync::atomic::{AtomicU32, Ordering};

use blaze_build_script::{facts, instr::OutputLimits, outputs::Outputs};
use blaze_proc_macro::{script_sandbox::Read, spawn_route::{FactPins, SpawnSetting}};

use crate::engine_script_reads::{ReadList, ReadsPlant};
use crate::engine_scripts::{KeptReads, ScriptRunner, with_runner};
use crate::scripts::{PooledScript, RanAs, admit_reads, outputs_record};

const SUBJECT: &str = "pkg-config\n--libs\n--cflags\nlibusb-1.0";

fn pins() -> FactPins {
    FactPins { clang_version_text: "clang version 23\n".into(), macos_product_version: "27.0".into(), macos_build_version: "26A434".into() }
}

fn libusb_probe(pins: &FactPins, alloc: blaze_own::Alloc<'_>) -> Read {
    let fact = facts::question("pkg-config", &["--libs", "--cflags", "libusb-1.0"]).expect("the sandbox handles this query");
    let reply = facts::answer(&fact, &pins.as_pins(), alloc).expect("the sandbox answered the query");
    assert_eq!(reply.code, 1, "the sandbox serves an empty pkg-config database");
    assert!(reply.stdout.as_str().is_empty());
    assert!(reply.stderr.as_str().contains("Package 'libusb-1.0', required by 'virtual:world', not found"));
    let subject = facts::subject(&fact, alloc);
    assert_eq!(subject.as_str(), SUBJECT, "the full argument vector names the observation");

    // Encode the reply the guest saw, independently of the replay reader.
    let mut bytes = reply.code.to_le_bytes().to_vec();
    for stream in [reply.stdout.as_bytes(), reply.stderr.as_bytes()] {
        bytes.extend_from_slice(&u32::try_from(stream.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(stream);
    }
    Read { subject: subject.as_str().to_owned(), digest: Some(blaze_ids::ContentId::of("blaze build-script spawn reply 1", &bytes)) }
}

struct UsbScript(AtomicU32);

impl ScriptRunner for UsbScript {
    fn run(&self, cx: &Cx, _: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: blaze_drive::Name, kept: Option<KeptReads>, _: &[(blaze_drive::Input, &[u8])]) -> PooledScript {
        self.0.fetch_add(1, Ordering::SeqCst);
        let erased = blaze_own::Erased::new(cx.frame());
        let alloc = erased.handle();
        let pins = pins();
        let observed = libusb_probe(&pins, alloc);
        let (plant, status) = kept.expect("the engine plans script reads");
        let mut checkout = ReadList::new_in(blaze_own::testing::heap().shared());
        // The simulated graph supplies the task. Admit its package reads under the fetched
        // tree contract, exactly as the libusb1-sys unit in tempo is admitted.
        let unkept = admit_reads(&[observed], RanAs::Wasm, &SpawnSetting::facts_only(pins), None, plant, status, &mut checkout, alloc).err();
        let cells: std::vec::Vec<_> = checkout.iter().map(|read| (read.name(), read.answer)).collect();
        let outputs = Outputs::read(b"cargo::rustc-cfg=vendored\n", "/out", OutputLimits::DEFAULT, alloc).expect("the script's cfg");
        let did = outputs_record(name, &cells, RanAs::Wasm, &outputs, "libusb1-sys 0.7.0");
        PooledScript { did: Ok(did), used: 0, ns: 0, checkout, unkept }
    }
}

#[test]
fn libusb_pkg_config_reply_certifies_the_second_request() {
    static SCRIPT: UsbScript = UsbScript(AtomicU32::new(0));
    let io = Io::simulated(SimWorld::new(564, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"usb\", \"app\"]\nresolver = \"2\"\n");
    write(&io, "usb/Cargo.toml", "[package]\nname = \"libusb1-sys\"\nversion = \"0.7.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
    write(&io, "usb/build.rs", "fn main() {}\n");
    write(&io, "usb/src/lib.rs", "#[cfg(vendored)]\npub fn usb_version() -> u8 { 1 }\n");
    write(&io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nlibusb1-sys = { path = \"../usb\" }\n");
    write(&io, "app/src/lib.rs", "pub fn version() -> u8 { libusb1_sys::usb_version() }\n");
    let pins = pins();
    let request = || crate::sweep::with_fact_inputs(&pins, None, || with_runner(&SCRIPT, || super::engine_check_tests::engine_from(&io, 1, false)));
    let (ids, first) = request();
    assert!(ids.is_empty(), "the consumer resolves the vendored implementation: {ids:?}");
    assert!(!first.certified);
    assert_eq!(SCRIPT.0.load(Ordering::SeqCst), 1);
    let owed = super::engine_answer_rows_tests::with_store(&io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("read pointer").expect("stored answer").expect("valid pointer");
        let question = format!("{}{SUBJECT}", crate::sweep::REPLY_FILES);
        let row = (0..pointer.leaves.len()).find(|&i| pointer.leaves.path(i) == question).expect("the certificate follows the exact pkg-config argv");
        let expected = libusb_probe(&pins, blaze_own::testing::heap().alloc());
        assert_eq!(Some(pointer.leaves.row(row).id), expected.digest);
        pointer.facts.owed
    });
    assert_eq!(owed, 0, "the stored answer owes no scripts or bodies");
    let (again_ids, again) = request();
    assert_eq!(again_ids, ids);
    assert!(again.certified);
    assert_eq!(again.entered, 0);
    assert_eq!(SCRIPT.0.load(Ordering::SeqCst), 1);
    println!("libusb pkg-config: exit 1, Scripts owed 0, second request certified, entered 0");
}

const CC_VERSION: &str = "cc-version\ncc\n--version";

/// What `cc --version` read in a wasm script (cc-rs's version check): the pinned clang's
/// version text, as spawn_route.rs `spawn_fact` records it.
fn cc_version_read(pins: &FactPins, alloc: blaze_own::Alloc<'_>) -> Read {
    let fact = facts::question("cc", &["--version"]).expect("`cc --version` is a derived fact");
    let reply = facts::answer(&fact, &pins.as_pins(), alloc).expect("the pins answer it");
    assert_eq!(reply.stdout.as_str(), pins.clang_version_text.as_str());
    let subject = facts::subject(&fact, alloc);
    assert_eq!(subject.as_str(), CC_VERSION);
    // The reply the guest saw, encoded independently of the replay reader (as `libusb_probe`).
    let mut bytes = reply.code.to_le_bytes().to_vec();
    for stream in [reply.stdout.as_bytes(), reply.stderr.as_bytes()] {
        bytes.extend_from_slice(&u32::try_from(stream.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(stream);
    }
    Read { subject: subject.as_str().to_owned(), digest: Some(blaze_ids::ContentId::of("blaze build-script spawn reply 1", &bytes)) }
}

/// The pinned clang the control's command runs under (`clang version 24`), [`pins`]' otherwise.
fn newer_pins() -> FactPins {
    FactPins { clang_version_text: "clang version 24\n".into(), ..pins() }
}

/// A fetched C crate's script that read `cc --version` (the 15 cc-class scripts of tempo): its
/// runs, and whether the command it runs in has [`newer_pins`]. A real script sees the reply of
/// the command's own pins; before this field it saw [`pins`]' whatever the command's were, so
/// under the control's newer clang its read disagreed with the command's own answer of the
/// reply file and the command stopped "changed while the command ran" (engine_scripts.rs, the
/// `given` check; t82: `Changed { paths: "script .. /blaze/facts/reply/cc-version.." }`).
struct CcScript(AtomicU32, core::sync::atomic::AtomicBool);

impl ScriptRunner for CcScript {
    fn run(&self, cx: &Cx, _: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: blaze_drive::Name, kept: Option<KeptReads>, _: &[(blaze_drive::Input, &[u8])]) -> PooledScript {
        self.0.fetch_add(1, Ordering::SeqCst);
        let erased = blaze_own::Erased::new(cx.frame());
        let alloc = erased.handle();
        let pins = if self.1.load(Ordering::SeqCst) { newer_pins() } else { pins() };
        let observed = cc_version_read(&pins, alloc);
        let (plant, status) = kept.expect("the engine plans script reads");
        let mut checkout = ReadList::new_in(blaze_own::testing::heap().shared());
        let unkept = admit_reads(&[observed], RanAs::Wasm, &SpawnSetting::facts_only(pins), None, plant, status, &mut checkout, alloc).err();
        let cells: std::vec::Vec<_> = checkout.iter().map(|read| (read.name(), read.answer)).collect();
        let outputs = Outputs::read(b"cargo::rustc-cfg=vendored\n", "/out", OutputLimits::DEFAULT, alloc).expect("the script's cfg");
        let did = outputs_record(name, &cells, RanAs::Wasm, &outputs, "zstd-sys 2.0.16");
        PooledScript { did: Ok(did), used: 0, ns: 0, checkout, unkept }
    }
}

/// tempo's check of 2026-11-21 (a0800): every script that read `cc-version` was "not kept, and
/// it runs again next command" (engine_script_reads.rs `NoCell::Kind`). Its reply is a function of
/// the pinned clang, so it is a keyed read: the second request runs no script and enters nothing.
#[test]
fn a_cc_version_reply_is_kept_and_the_second_request_runs_no_script() {
    static SCRIPT: CcScript = CcScript(AtomicU32::new(0), core::sync::atomic::AtomicBool::new(false));
    let io = Io::simulated(SimWorld::new(565, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"zstd\", \"app\"]\nresolver = \"2\"\n");
    write(&io, "zstd/Cargo.toml", "[package]\nname = \"zstd-sys\"\nversion = \"2.0.16\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
    write(&io, "zstd/build.rs", "fn main() {}\n");
    write(&io, "zstd/src/lib.rs", "#[cfg(vendored)]\npub fn zstd_version() -> u8 { 1 }\n");
    write(&io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nzstd-sys = { path = \"../zstd\" }\n");
    write(&io, "app/src/lib.rs", "pub fn version() -> u8 { zstd_sys::zstd_version() }\n");
    let pins = pins();
    let request = || crate::sweep::with_fact_inputs(&pins, None, || with_runner(&SCRIPT, || super::engine_check_tests::engine_from(&io, 1, false)));
    let (ids, first) = request();
    assert!(ids.is_empty(), "the consumer sees the script's cfg: {ids:?}");
    assert_eq!(SCRIPT.0.load(Ordering::SeqCst), 1);
    let owed = super::engine_answer_rows_tests::with_store(&io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("read pointer").expect("stored answer").expect("valid pointer");
        let question = format!("{}{CC_VERSION}", crate::sweep::REPLY_FILES);
        let row = (0..pointer.leaves.len()).find(|&i| pointer.leaves.path(i) == question).expect("the certificate follows `cc --version`");
        assert_eq!(Some(pointer.leaves.row(row).id), cc_version_read(&pins, blaze_own::testing::heap().alloc()).digest);
        pointer.facts.owed
    });
    assert_eq!(owed, 0, "the stored answer owes no scripts or bodies");
    assert!(!first.certified);
    let (again_ids, again) = request();
    assert_eq!(again_ids, ids);
    assert!(again.certified);
    assert_eq!(again.entered, 0);
    assert_eq!(SCRIPT.0.load(Ordering::SeqCst), 1, "a second command runs no script");
    // Control (rule 23): another pinned clang is another reply: the read moves and the script
    // runs again.
    let newer = newer_pins();
    SCRIPT.1.store(true, Ordering::SeqCst);
    let (_, moved) = crate::sweep::with_fact_inputs(&newer, None, || with_runner(&SCRIPT, || super::engine_check_tests::engine_from(&io, 1, false)));
    assert!(!moved.certified);
    assert_eq!(SCRIPT.0.load(Ordering::SeqCst), 2, "a new clang re-runs the script");
    println!("cc --version: kept as a reply file; second request certified, entered 0; a new clang pin re-runs it");
}

#[test]
fn incomplete_or_mismatched_pkg_config_observations_stay_unkept() {
    let heap = blaze_own::testing::heap();
    let pins = pins();
    let good = libusb_probe(&pins, heap.alloc());
    let setting = SpawnSetting::facts_only(pins);
    for (subject, digest) in [
        (SUBJECT, None),
        (SUBJECT, Some(blaze_ids::ContentId([0; 32]))),
        (CC_VERSION, None),
        (CC_VERSION, Some(blaze_ids::ContentId([0; 32]))),
        ("pkg-config\n--unsupported", good.digest),
        ("pkg-config\n--version", good.digest),
        ("some-other-tool\n--version", good.digest),
    ] {
        let observed = Read { subject: subject.into(), digest };
        for member in [None, Some("")] {
            let mut cells = ReadList::new_in(heap.shared());
            let why = admit_reads(core::slice::from_ref(&observed), RanAs::Wasm, &setting, member, ReadsPlant::None, None, &mut cells, heap.alloc())
                .expect_err("an incomplete or unsupported observation cannot keep the script");
            assert_eq!(why.first_rejected().1, subject);
        }
    }
    println!("absent, mismatched and unsupported tool replies stay unkept for fetched and member scripts");
}

#[cfg(unix)]
mod searched_tool {
    use super::*;
    use blaze_proc_macro::native_facts;
    use blaze_proc_macro::script_sandbox::{RealRoot, Tree, TreeRead, dir_tree};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    // Only shell builtins. The grant contains the executable, the env source, and two
    // search directories. stderr traces the precise sequence of searches and file reads.
    const PROGRAM: &str = r#"#!/bin/sh
[ "$#" = 3 ] && [ "$1" = --libs ] && [ "$2" = --cflags ] && [ "$3" = libusb-1.0 ] || exit 80
for dir in "$PKG_CONFIG_PATH" "$PKG_CONFIG_LIBDIR"; do
    printf 'dir:%s\n' "$dir" >&2
    pc="$dir/$3.pc"
    if [ -f "$pc" ]; then
        printf 'file:%s\n' "$pc" >&2
        IFS= read -r flavor < "$pc" || exit 81
        printf 'cargo::rustc-cfg=probe_v1_%s_%s\n' "$PKG_CONFIG_SYSROOT_DIR" "$flavor"
        exit 0
    fi
done
exit 1
"#;

    struct Directory(PathBuf);

    impl Drop for Directory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove the tool fixture");
        }
    }

    struct SearchedTool {
        root: PathBuf,
        runs: AtomicU32,
        drop_listing: bool,
    }

    impl ScriptRunner for SearchedTool {
        fn checkout_tree(&self) -> Option<Tree> {
            Some(dir_tree(&RealRoot::resolve(&self.root).expect("the fixture's folder resolves")))
        }

        fn run(&self, cx: &Cx, _: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: blaze_drive::Name, kept: Option<KeptReads>, _: &[(blaze_drive::Input, &[u8])]) -> PooledScript {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let env_file = "sysroot";
            let exe = "bin/pkg-config";
            let tree = dir_tree(&RealRoot::resolve(&self.root).expect("the fixture's folder resolves"));
            let image = native_facts::image_digest(&self.root.join(exe)).expect("capture the executable this run starts");
            let mut observed = vec![Read { subject: native_facts::IMAGE_SUBJECT.to_owned(), digest: Some(image) }];
            let mut record = |kind: TreeRead, prefix: &str, path: &str| {
                let digest = kind.answer(&tree, path).expect("read from the granted tree");
                observed.push(Read { subject: format!("{prefix}:{path}"), digest });
            };
            record(TreeRead::File, "file", env_file);
            let env = fs::read_to_string(self.root.join(env_file)).expect("read PKG_CONFIG_SYSROOT_DIR");
            let process = std::process::Command::new(self.root.join(exe))
                .args(["--libs", "--cflags", "libusb-1.0"])
                .env_clear()
                .env("PKG_CONFIG_SYSROOT_DIR", env.trim_end())
                .env("PKG_CONFIG_PATH", "preferred")
                .env("PKG_CONFIG_LIBDIR", "system")
                .current_dir(&self.root)
                .output().expect("run the fixture's pkg-config");
            assert!(process.status.success(), "pkg-config failed: {}", String::from_utf8_lossy(&process.stderr));
            for entry in std::str::from_utf8(&process.stderr).expect("the trace is UTF-8").lines() {
                let (op, path) = entry.split_once(':').expect("each observation names its operation");
                match op {
                    "dir" if self.drop_listing => {}
                    "dir" => record(TreeRead::Listing, op, path),
                    "file" => record(TreeRead::File, op, path),
                    _ => panic!("unexpected observation {entry}"),
                }
            }
            let erased = blaze_own::Erased::new(cx.frame());
            let alloc = erased.handle();
            let (plant, status) = kept.expect("the request keeps reads");
            let mut checkout = ReadList::new_in(blaze_own::testing::heap().shared());
            let unkept = admit_reads(&observed, RanAs::Native, &SpawnSetting::facts_only(pins()), Some(""), plant, status, &mut checkout, alloc).err();
            assert!(unkept.is_none(), "the registered grant accounts for each read: {unkept:?}");
            let cells: std::vec::Vec<_> = checkout.iter().map(|read| (read.name(), read.answer)).collect();
            let outputs = Outputs::read(&process.stdout, "/out", OutputLimits::DEFAULT, alloc).expect("use what pkg-config printed");
            let did = outputs_record(name, &cells, RanAs::Native, &outputs, "probe 1.0.0");
            PooledScript { did: Ok(did), used: 0, ns: 0, checkout, unkept }
        }
    }

    fn fixture(tag: &str, drop_listing: bool) -> (Io, Directory, &'static SearchedTool) {
        let root = std::env::temp_dir().join(format!("blaze-ht1-{tag}-{}", std::process::id()));
        fs::create_dir(&root).expect("create the fixture directory");
        let dir = Directory(root.clone());
        fs::create_dir(root.join("bin")).unwrap();
        fs::create_dir(root.join("system")).unwrap();
        fs::write(root.join("system/libusb-1.0.pc"), "lower\n").unwrap();
        fs::write(root.join("sysroot"), "one\n").unwrap();
        fs::write(root.join("bin/pkg-config"), PROGRAM).unwrap();
        fs::set_permissions(root.join("bin/pkg-config"), fs::Permissions::from_mode(0o755)).unwrap();
        let io = Io::simulated(SimWorld::new(565, Limits::USUAL));
        write(&io, "Cargo.toml", "[package]\nname = \"probe\"\nversion = \"1.0.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
        write(&io, "build.rs", "fn main() {}\n");
        write(&io, "src/lib.rs", "pub fn probe() -> u8 { 1 }\n");
        let runner = std::boxed::Box::leak(std::boxed::Box::new(SearchedTool { root, runs: AtomicU32::new(0), drop_listing }));
        (io, dir, runner)
    }

    fn request(io: &Io, runner: &'static SearchedTool, fresh: bool) -> engine_check::Checked {
        let exe = runner.root.join("bin/pkg-config");
        let (ids, checked) = crate::sweep::with_fact_inputs(&pins(), Some(&exe), || {
            with_runner(runner, || super::super::engine_check_tests::engine_from(io, 1, fresh))
        });
        assert!(ids.is_empty(), "the probe project type-checks: {ids:?}");
        checked
    }

    fn script_out(io: &Io) -> blaze_ids::ContentId {
        super::super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
            let name = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
            let pointer = crate::engine_answer::load_pointer(cx, store, &name).expect("read pointer").expect("stored answer").expect("valid pointer");
            let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("read answer").expect("valid answer");
            let mut scripts = kept.answer.rows.iter().filter(|(name, _)| name.kind == blaze_drive::Kind::Script);
            let out = scripts.next().expect("the build script has a record").1.out;
            assert!(scripts.next().is_none(), "only one build script in the fixture");
            out
        })
    }

    fn shadow(root: &Path) {
        fs::create_dir(root.join("preferred")).unwrap();
        fs::write(root.join("preferred/libusb-1.0.pc"), "upper\n").unwrap();
    }

    #[test]
    fn executable_environment_and_search_files_reenter_the_script() {
        let (io, dir, tool) = fixture("edits", false);
        let cold = request(&io, tool, false);
        assert!(!cold.certified);
        assert_eq!(tool.runs.load(Ordering::SeqCst), 1);
        let mut last = script_out(&io);
        for round in 0..=4 {
            let again = request(&io, tool, false);
            assert!(again.certified);
            assert_eq!(again.entered, 0);
            assert_eq!(tool.runs.load(Ordering::SeqCst), round + 1);
            match round {
                0 => fs::write(dir.0.join("bin/pkg-config"), PROGRAM.replace("probe_v1_", "probe_v2_")).unwrap(),
                1 => fs::write(dir.0.join("sysroot"), "two\n").unwrap(),
                2 => fs::write(dir.0.join("system/libusb-1.0.pc"), "other\n").unwrap(),
                3 => shadow(&dir.0),
                _ => break,
            }
            let moved = request(&io, tool, false);
            assert!(!moved.certified);
            assert_eq!(tool.runs.load(Ordering::SeqCst), round + 2, "edit {round} entered the build script");
            let out = script_out(&io);
            assert_ne!(out, last, "the executed tool changed the script output after edit {round}");
            last = out;
        }
        println!("pkg-config: executable, environment, file edit and shadowing file each reentered one script; no-op entered 0");
    }

    #[test]
    fn dropping_the_search_directory_observation_is_caught() {
        let (io, dir, tool) = fixture("control", true);
        request(&io, tool, false);
        let old = script_out(&io);
        shadow(&dir.0);
        assert!(request(&io, tool, false).certified, "the planted omission misses the new higher-priority package");
        assert_eq!(tool.runs.load(Ordering::SeqCst), 1);
        assert_eq!(script_out(&io), old);
        request(&io, tool, true);
        assert_eq!(tool.runs.load(Ordering::SeqCst), 2);
        assert_ne!(script_out(&io), old, "a fresh process reads the shadowing .pc file and produces different bytes");
        println!("planted missing search-directory observation caught by a fresh pkg-config reply");
    }

    fn saved_rows(io: &Io, cells: &ReadList) -> std::vec::Vec<u8> {
        let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
        let mut rows = crate::sweep::Rows::new_in(cx.frame());
        crate::engine_script_reads::keep_rows(None, cells.iter().map(|read| (read.kind, read.path.as_str(), read.answer)), |_| true, &mut rows);
        crate::sweep::Leaves::bytes_in(&rows, cx.frame()).unwrap().to_vec()
    }

    fn changed(io: &Io, bytes: &[u8], pins: &FactPins, exe: &Path) -> std::vec::Vec<std::string::String> {
        use crate::sweep::{Follow, FromMeta, Leaves, Plant};
        let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
        let (last, older) = Leaves::of(bytes).unwrap();
        assert!(older.is_none());
        let root = io.need_fs().unwrap().open_dir(&cx, &bstd::path::Path::new("ws").unwrap()).unwrap();
        let follow = Follow { stamps: FromMeta::now(&io.need_clock().unwrap()), plant: Plant::None };
        crate::sweep::with_fact_inputs(pins, Some(exe), || {
            // A silent checkout journal cannot vouch for external machine inputs.
            let quiet = vec![false; last.len()];
            let (now, _) = follow.follow_marked(&cx, &root, None, &last, Some(&quiet), cx.frame()).unwrap();
            (0..last.len()).filter(|&i| now.changed(i)).map(|i| last.path(i).to_owned()).collect()
        })
    }

    #[test]
    fn native_receipt_stands_on_the_deployed_shim_image() {
        use blaze_build_script::fact_shim::LOG_FILE;

        let (io, dir, _) = fixture("native-receipt", false);
        let exe = dir.0.join("bin/pkg-config");
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let pins = pins();
        let setting = SpawnSetting::facts_only(pins.clone());
        let run = native_facts::prepare(exe.to_str().unwrap(), dir.0.to_str().unwrap(), "unit", Some(&pins), alloc).unwrap();
        let log = Path::new(&run.log_dir).join(LOG_FILE);
        let read = libusb_probe(&pins, alloc);
        let mut cells = ReadList::new_in(heap.shared());

        // Old question-only logs did not record the bytes the child actually received.
        fs::write(&log, format!("a{SUBJECT}\0")).unwrap();
        let old = native_facts::reads(&run, Some(&pins), alloc).unwrap();
        let rejected = admit_reads(&old.reads, RanAs::Native, &setting, None, ReadsPlant::None, None, &mut cells, alloc).unwrap_err();
        assert_eq!(rejected.first_rejected().1, SUBJECT);

        let hex: std::string::String = read.digest.unwrap().0.iter().map(|byte| format!("{byte:02x}")).collect();
        fs::write(&log, format!("apkg-config-reply-v1\n{hex}\n{SUBJECT}\0")).unwrap();
        let receipt = native_facts::reads(&run, Some(&pins), alloc).unwrap();
        cells.clear();
        admit_reads(&receipt.reads, RanAs::Native, &setting, None, ReadsPlant::None, None, &mut cells, alloc).expect("image and observed reply");
        assert_eq!(cells.len(), 3, "the reply, the pins and the image");
        let bytes = saved_rows(&io, &cells);
        assert!(changed(&io, &bytes, &pins, &exe).is_empty());
        fs::write(&exe, PROGRAM.replace("probe_v1_", "probe_v2_")).unwrap();
        assert_eq!(changed(&io, &bytes, &pins, &exe), [crate::sweep::SHIM_FILE], "changing the source image moves its saved cell");
        println!("native receipt: legacy question unkept, unchanged image stands, replaced image moves despite quiet journal");
    }

    #[test]
    fn truncated_native_log_still_follows_image_and_pins() {
        let (io, dir, _) = fixture("native-empty-log", false);
        let exe = dir.0.join("bin/pkg-config");
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let pins = pins();
        let run = native_facts::prepare(exe.to_str().unwrap(), dir.0.to_str().unwrap(), "unit", Some(&pins), alloc).unwrap();
        let log = Path::new(&run.log_dir).join(blaze_build_script::fact_shim::LOG_FILE);
        fs::write(&log, format!("a{SUBJECT}\0")).unwrap();
        fs::write(&log, b"").unwrap();
        let empty = native_facts::reads(&run, Some(&pins), alloc).unwrap();
        let setting = SpawnSetting::facts_only(pins.clone());
        let mut cells = ReadList::new_in(heap.shared());
        admit_reads(&empty.reads, RanAs::Native, &setting, None, ReadsPlant::None, None, &mut cells, alloc).unwrap();
        assert_eq!(cells.len(), 2, "even a truncated log retains the image and its configuration");
        let pins_cell = cells.iter().find(|cell| cell.path.as_str() != crate::sweep::SHIM_FILE).expect("the pins cell").path.as_str();
        let bytes = saved_rows(&io, &cells);
        assert!(changed(&io, &bytes, &pins, &exe).is_empty());
        let mut other = pins.clone();
        other.macos_product_version = "27.1".to_owned();
        assert_eq!(changed(&io, &bytes, &other, &exe), [pins_cell]);
        fs::write(&exe, PROGRAM.replace("probe_v1_", "probe_v2_")).unwrap();
        assert_eq!(changed(&io, &bytes, &pins, &exe), [crate::sweep::SHIM_FILE]);
        println!("truncated native log: both a pin change and an image change invalidate the saved answer");
    }
}
