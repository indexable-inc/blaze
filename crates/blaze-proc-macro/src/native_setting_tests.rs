//! The native setting's producer (D112, brief N1) and what it reaches on the real path.
//!
//! What is right: a command with the pinned toolchain unpacked on its shelf and an SDK whose
//! `SDKSettings.json` gives a `Version` has a native setting, and a script placed native
//! ([`crate::native_unit::placed`]) then gets as far as looking for its host binary
//! (`script_unit::module_of_native`): with none in the store, the refusal is `NoModule`, not
//! `NoNativeSetting`. A missing shelf is refused naming the shelf's path. Nothing here runs a
//! script, clang or the sandbox: the fake home holds empty folders and one settings file.
//!
//! Expected from `lanecheck.sh blaze-proc-macro test native_setting`:
//! `test blaze-proc-macro: exit 0; 3 passed, 0 failed`; the last test prints
//! "native: with a setting NoModule for zstd-sys 1.0.0, without NoNativeSetting".

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_build_script::{key::CMode, toolchain::Mode};
use blaze_manifest::lock::LockId;
use blaze_own::{AVec, Alloc};
use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::PackageEdition;

use crate::{
    native_setting::{NATIVE_RUN_DEADLINE, NATIVE_WORK_DIR, NativeSettingError, OwnedNativeSetting, SHELF_DIR, command_native_setting},
    native_unit::{NativeSetting, NativeUnitError},
    script::ScriptRuntime,
    script_sandbox::{ScriptRoot, Tree},
    script_unit::{Setting, UnitScriptError, outputs},
    spawn_route::SpawnSetting,
};

const HOST: &str = "aarch64-apple-darwin";
/// The aarch64-apple-darwin row's sha256 in pins/clang.json.
const PIN_HEX: &str = "5d3179db39adeca5b17a34d82708fe976bc60e055b05fe557878fb3fb64d39ef";

/// A fresh folder under the system's temporary one, by this process and `name`.
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-native-setting-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a fresh test folder");
    dir
}

/// A home with the pin's shelf (when `shelf`) and an SDK folder giving `Version` 15.2.
fn fake_home(name: &str, shelf: bool) -> (std::string::String, std::string::String) {
    let home = folder(name);
    if shelf {
        // nm-row: every member the setting looks for (toolchain::SHELF_MEMBERS), empty.
        let dir = home.join(SHELF_DIR).join(PIN_HEX);
        for member in blaze_build_script::toolchain::SHELF_MEMBERS {
            let path = dir.join(member);
            if member == "lib/clang" {
                std::fs::create_dir_all(&path).expect("the resource folder");
            } else {
                std::fs::create_dir_all(path.parent().expect("bin/ or lib/")).expect("the shelf folder");
                std::fs::write(&path, b"").expect("a shelf member");
            }
        }
    }
    let sdk = home.join("MacOSX.sdk");
    std::fs::create_dir_all(&sdk).expect("the SDK folder");
    std::fs::write(sdk.join("SDKSettings.json"), r#"{"CanonicalName":"macosx15.2","Version":"15.2"}"#).expect("SDKSettings.json");
    (home.to_str().expect("a UTF-8 temp folder").to_owned(), sdk.to_str().expect("a UTF-8 temp folder").to_owned())
}

fn produce<'a>(home: &str, sdk: &str, alloc: Alloc<'a>) -> Result<OwnedNativeSetting<'a>, NativeSettingError<'a>> {
    command_native_setting(home, sdk, "test", HOST, alloc)
}

/// The command's jobserver under the setting's work folder, two workers.
fn jobserver(owned: &OwnedNativeSetting<'_>) -> crate::jobserver::Jobserver {
    let opts = crate::jobserver::JobserverOpts { work: owned.work.as_str(), workers: core::num::NonZeroU32::new(2).expect("2"), id: std::process::id() };
    crate::jobserver::Jobserver::new(opts).unwrap_or_else(|e| panic!("{e}"))
}

/// The producer names the pin's shelf, the SDK's version and the command's own work folder.
/// A producer that ignored the pin table (any other folder name) or the SDK's settings fails.
#[test]
fn the_native_setting_names_the_pins_shelf_the_sdks_version_and_the_commands_work_folder() {
    let alloc = blaze_own::testing::heap().alloc();
    let (home, sdk) = fake_home("produced", true);
    let owned = produce(&home, &sdk, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(owned.shelf.as_str().rsplit('/').next(), Some(PIN_HEX));
    assert_eq!(owned.sdk_version.as_str(), "15.2");
    assert!(owned.sdk.as_str().ends_with("/MacOSX.sdk"), "{}", owned.sdk.as_str());
    assert!(owned.work.as_str().ends_with(&format!("{NATIVE_WORK_DIR}/test")), "{}", owned.work.as_str());
    assert!(std::fs::metadata(owned.work.as_str()).map(|m| m.is_dir()).unwrap_or(false), "the work folder is made");
    let jobs = jobserver(&owned);
    let setting: NativeSetting<'_> = owned.setting(&jobs);
    assert_eq!(setting.deadline, NATIVE_RUN_DEADLINE);
    assert!(setting.jobs.fifo().starts_with(owned.work.as_str()), "the fifo is the command's own: {}", setting.jobs.fifo());
    jobs.end().unwrap_or_else(|e| panic!("{e}"));
    let _ = std::fs::remove_dir_all(&home);
}

/// No shelf: refused naming the shelf's path, never another toolchain (rule 3).
#[test]
fn a_missing_shelf_is_refused_naming_its_path() {
    let alloc = blaze_own::testing::heap().alloc();
    let (home, sdk) = fake_home("no-shelf", false);
    match produce(&home, &sdk, alloc) {
        Err(NativeSettingError::NoShelf { path, .. }) => {
            assert_eq!(path.as_str(), format!("{home}/{SHELF_DIR}/{PIN_HEX}"));
        }
        Err(other) => panic!("not the missing shelf: {other}"),
        Ok(owned) => panic!("a setting with no shelf: {}", owned.shelf.as_str()),
    }
    let _ = std::fs::remove_dir_all(&home);
}

fn t(s: &str) -> Text<'static> {
    Text::from_str_in(s, blaze_own::testing::heap().alloc())
}

fn unit(name: &str, kind: UnitKind<'static>, deps: &[u32], build_script: Option<u32>) -> Unit<'static> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut d = AVec::new_in(alloc);
    for u in deps {
        d.push(UnitDep { name: t("dep"), unit: UnitId(*u) });
    }
    Unit {
        package: LockId(0),
        name: t(name),
        version: t("1.0.0"),
        kind,
        for_host: true,
        edition: PackageEdition::of_manifest("2021").expect("an edition"),
        crate_name: t(name),
        root: t("build.rs"),
        features: AVec::new_in(alloc),
        cfg: AVec::new_in(alloc),
        deps: d,
        build_script: build_script.map(UnitId),
        links: None,
    }
}

/// A `build` command's setting over `store` with `native` (no spawns, no try-compile answers).
fn setting<'s>(store: &'s Path, native: Option<NativeSetting<'s>>) -> Setting<'s> {
    Setting {
        store,
        target: HOST,
        host: HOST,
        table: Arc::new(Vec::new()),
        probes: None,
        c_mode: CMode::Built,
        mode: Mode::Build,
        native,
        spawn_setting: Arc::new(SpawnSetting::off()),
    }
}

/// zstd-sys's script (its closure holds `cc`) is placed native for `build`; with the
/// producer's setting the real path (`script_unit::outputs`) reaches the host binary's lookup
/// and, the store holding none, refuses with `NoModule`; with no setting, `NoNativeSetting`. A
/// setting that never reached the native path (left `None` by the command) fails the first
/// match.
#[test]
fn a_native_placed_script_with_a_setting_reaches_its_binary_lookup() {
    let alloc = blaze_own::testing::heap().alloc();
    let (home, sdk) = fake_home("reaches", true);
    let owned = produce(&home, &sdk, alloc).unwrap_or_else(|e| panic!("{e}"));

    let mut units = AVec::new_in(alloc);
    units.push(unit("zstd-sys", UnitKind::BuildScript, &[1], None)); // 0
    units.push(unit("cc", UnitKind::Lib, &[], None)); // 1
    units.push(unit("zstd-sys", UnitKind::Lib, &[], Some(0))); // 2
    let graph = UnitGraph { units, resolver: Resolver::V3, meta: AVec::new_in(alloc), left_out: AVec::new_in(alloc) };

    let store = folder("reaches-store");
    let runtime = ScriptRuntime::new(&store).unwrap_or_else(|e| panic!("{e}"));
    let setting_with = |native| setting(&store, native);
    let run = |setting: &Setting<'_>| {
        let tree = Tree::Mem { files: Arc::new(Vec::new()), git: None };
        outputs(&runtime, setting, &graph, UnitId(0), tree, ScriptRoot::package(), &[], alloc)
    };

    let jobs = jobserver(&owned);
    let with = match run(&setting_with(Some(owned.setting(&jobs)))) {
        Err(UnitScriptError::Native(NativeUnitError::NoModule { package, .. })) => package,
        Err(other) => panic!("with a native setting, not the binary's lookup: {other}"),
        Ok(_) => panic!("a run with no host binary in the store"),
    };
    assert_eq!(with, "zstd-sys 1.0.0");
    match run(&setting_with(None)) {
        Err(UnitScriptError::Native(NativeUnitError::NoNativeSetting { .. })) => {}
        Err(other) => panic!("with no native setting, not NoNativeSetting: {other}"),
        Ok(_) => panic!("a native run with no setting"),
    }
    println!("native: with a setting NoModule for {with}, without NoNativeSetting");
    jobs.end().unwrap_or_else(|e| panic!("{e}"));
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&store);
}
