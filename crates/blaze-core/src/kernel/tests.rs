use super::*;

#[test]
fn releases_parse_to_their_major_and_minor() {
    assert_eq!(parse("7.3.1"), Some(v(7, 3)));
    assert_eq!(parse("7.2"), Some(v(7, 2)));
    assert_eq!(parse("6.8.0-1070-azure"), Some(v(6, 8)));
    assert_eq!(parse("7.10-rc1"), Some(v(7, 10)));
    assert_eq!(parse("2.6.60"), Some(v(2, 6)));
    assert_eq!(parse("seven"), None);
    assert_eq!(parse("7"), None);
}

#[test]
fn the_floor_admits_7_3_and_later_and_refuses_older() {
    assert_eq!(admits("7.3.1"), Ok(v(7, 3)));
    assert_eq!(admits("7.4.0"), Ok(v(7, 4)));
    assert_eq!(admits("8.0.1"), Ok(v(8, 0)));
    for old in ["7.2.11", "7.1.9", "6.8.0-1070-azure", "6.11.0-1018-azure", "5.15.0"] {
        assert!(matches!(admits(old), Err(TooOld::Below { .. })), "{old}");
    }
    assert!(matches!(admits("garbage"), Err(TooOld::Unread { .. })));
}

/// The refusal names the running kernel, the floor, every feature that needs it and the fix.
#[test]
fn the_refusal_names_the_kernel_the_floor_the_features_and_the_fix() {
    let said = admits("6.8.0-1070-azure").unwrap_err().to_string();
    assert!(said.contains("Linux 6.8.0-1070-azure"), "{said}");
    assert!(said.contains("Linux 7.3 or later"), "{said}");
    for feature in FEATURES {
        assert!(said.contains(feature.name), "{said} lacks {}", feature.name);
    }
    assert!(said.contains("ix VM"), "{said}");
    assert!(said.contains("HWE"), "{said}");
}

/// A feature newer than the floor would be used on a kernel that lacks it.
#[test]
fn no_feature_is_newer_than_the_floor() {
    for feature in FEATURES {
        assert!(feature.since <= FLOOR, "{} needs Linux {}", feature.name, feature.since);
    }
}

/// A faked older kernel: the `UNAME26` personality makes `uname` say 2.6.x (as
/// `setarch --uname-2.6` does). A personality is the calling task's, so set on a thread of
/// its own it fakes the release for that thread alone; the check run there must refuse it.
#[cfg(target_os = "linux")]
#[test]
#[allow(unsafe_code, reason = "personality(2) on a thread of the test's own")]
fn the_check_refuses_a_faked_older_kernel() {
    /// `linux/personality.h`'s `UNAME26`.
    const UNAME26: libc::c_ulong = 0x0020000;
    let real = running().unwrap();
    assert!(check().is_ok(), "the test runs on Linux {real}, below the floor");
    let refused = std::thread::spawn(|| {
        // SAFETY: 0xffffffff only reads the personality; no pointers.
        let current = unsafe { libc::personality(0xffff_ffff) };
        assert!(current >= 0, "personality: {}", std::io::Error::last_os_error());
        // SAFETY: sets this thread's personality; no pointers. The thread ends after.
        let set = unsafe { libc::personality(current as libc::c_ulong | UNAME26) };
        assert!(set >= 0, "personality: {}", std::io::Error::last_os_error());
        (running().unwrap(), check())
    })
    .join()
    .unwrap();
    let (faked, checked) = refused;
    assert!(faked.starts_with("2.6."), "UNAME26 did not fake the release: {faked}");
    let Err(TooOld::Below { running, version }) = checked else {
        panic!("Linux {faked} was not refused: {checked:?}")
    };
    assert_eq!((running.as_str(), version.major), (faked.as_str(), 2));
    // The thread's personality did not leak to this one.
    assert_eq!(super::running().unwrap(), real);
}
