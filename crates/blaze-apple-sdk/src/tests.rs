//! The resolution as a pure function of what the edge read: `SDKROOT`'s value, xcrun's answer
//! or failure, and the settings file of the one folder chosen.

use super::{Found, NotSdk, SdkRefused, Unread, Version, XcrunFailed, resolve, settings_version};

const SETTINGS_27_2: &str = r#"{"CanonicalName":"macosx27.2","VersionMap":{"Version":"1.0","macOS_iOSMac":{"27.2":"27.2"}},"Version":"27.2"}"#;

/// A machine whose settings file is `SETTINGS_27_2` at `/sdk/a` and `/sdk/b`, `{}` at
/// `/sdk/empty`, and missing elsewhere.
fn settings(folder: &str) -> Result<&'static str, Unread<'static>> {
    match folder {
        "/sdk/a" | "/sdk/b" => Ok(SETTINGS_27_2),
        "/sdk/empty" => Ok("{}"),
        _ => Err(Unread { why: "No such file or directory (os error 2)" }),
    }
}

fn never() -> Result<&'static str, XcrunFailed<'static>> {
    panic!("xcrun was asked although SDKROOT is set")
}

#[test]
fn sdkroot_set_to_an_sdk_folder_is_taken_and_xcrun_is_not_asked() {
    let sdk = resolve(Some("/sdk/a"), never, settings).expect("SDKROOT names an SDK");
    assert_eq!((sdk.folder(), sdk.found(), sdk.version_text()), ("/sdk/a", Found::SdkRoot, "27.2"));
    assert_eq!(sdk.identity(), Version::new(27, 2, 0).expect("in range"));
    println!("SDKROOT taken");
}

#[test]
fn sdkroot_set_to_no_sdk_folder_is_refused_naming_the_value() {
    let refused = resolve(Some("/nowhere"), never, settings).expect_err("no SDK there");
    assert_eq!(refused, SdkRefused::SdkRoot { value: "/nowhere", why: NotSdk::Unread(Unread { why: "No such file or directory (os error 2)" }) });
    let words = format!("{refused}");
    assert!(words.contains("SDKROOT=/nowhere") && words.contains("SDKSettings.json"), "{words}");
    // An SDK's name is xcrun's form, and a folder with no version is no SDK.
    assert_eq!(resolve(Some("macosx"), never, settings), Err(SdkRefused::SdkRoot { value: "macosx", why: NotSdk::NotAbsolute }));
    assert_eq!(resolve(Some("/sdk/empty"), never, settings), Err(SdkRefused::SdkRoot { value: "/sdk/empty", why: NotSdk::NoVersion }));
    println!("SDKROOT refused by its value");
}

#[test]
fn sdkroot_unset_takes_the_folder_xcrun_printed() {
    for unset in [None, Some("")] {
        let sdk = resolve(unset, || Ok("/sdk/b\n"), settings).expect("xcrun names an SDK");
        assert_eq!((sdk.folder(), sdk.found()), ("/sdk/b", Found::Xcrun));
        assert_eq!(sdk.identity(), Version::new(27, 2, 0).expect("in range"));
    }
    let refused = resolve(None, || Ok("/gone\n"), settings).expect_err("xcrun printed no SDK folder");
    assert!(matches!(refused, SdkRefused::Xcrun { printed: "/gone", why: NotSdk::Unread(_) }), "{refused:?}");
    println!("xcrun's folder taken");
}

#[test]
fn sdkroot_unset_and_xcrun_failing_names_both_and_how_to_set_it() {
    let failures = [
        XcrunFailed::Exited { status: 1, said: "xcrun: error: unable to find utility" },
        XcrunFailed::Expired { after: super::XCRUN_DEADLINE },
        XcrunFailed::Failed { why: "No such file or directory (os error 2)" },
        XcrunFailed::NotMacos,
    ];
    for xcrun in failures {
        let refused = resolve(None, || Err(xcrun), |_| panic!("no folder to read")).expect_err("no SDK");
        assert_eq!(refused, SdkRefused::Neither { xcrun });
        let words = format!("{refused}");
        assert!(words.contains("SDKROOT is not set") && words.contains("xcrun --sdk macosx --show-sdk-path"), "{words}");
        assert!(words.contains("export SDKROOT="), "the refusal says how to set SDKROOT: {words}");
    }
    println!("both attempts named");
}

#[test]
fn versions_and_the_sdks_settings_are_read_as_rustc_reads_them() {
    let nibbles = |text: &str| Version::parse(text).map(Version::nibbles);
    assert_eq!(nibbles("11"), Some(0x000b_0000));
    assert_eq!(nibbles("10.15.7"), Some(0x000a_0f07));
    for bad in ["", "11.", "11.0.0.0", "x", "11.256", "+11", "11 .0", "65536"] {
        assert_eq!(nibbles(bad), None, "{bad}");
    }
    let json = r#"{ "Nested": {"Version": "1"}, "List": ["Version", {"Version": "2"}], "Name": "Version", "Version" : "15.2" }"#;
    assert_eq!(settings_version(json), Some(("15.2", Version::new(15, 2, 0).expect("in range"))));
    assert_eq!(settings_version(r#"{"Version": 15}"#), None);
    assert_eq!(settings_version(r#"{"Other": "a\"Version\": \"9\""}"#), None);
    // A `VersionMap`'s nested `Version` is never the SDK's.
    assert_eq!(settings_version(SETTINGS_27_2).map(|it| it.0), Some("27.2"));
    assert_eq!(format!("{}", Version::new(10, 15, 7).expect("in range")), "10.15.7");
    assert_eq!(format!("{}", Version::new(27, 0, 0).expect("in range")), "27.0");
    println!("versions read");
}
