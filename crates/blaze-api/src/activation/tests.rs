use super::{ActivationError, FIRST_FD, Launchd, Listen, SOCKET, launchd, systemd};

const OWN: u32 = 4242;

fn listen<'a>(pid: &'a str, fds: &'a str, names: Option<&'a str>) -> Listen<'a> {
    Listen { pid: Some(pid), fds: Some(fds), names }
}

fn said(result: Result<i32, ActivationError>) -> String {
    result.expect_err("refused").what
}

#[test]
fn one_socket_for_this_process_named_blaze() {
    assert_eq!(systemd(listen("4242", "1", Some("blaze")), OWN, SOCKET), Ok(FIRST_FD));
}

#[test]
fn a_valid_activation_is_read_without_allocating() {
    let fd = blaze_alloc_count::assert_at_most(0, "systemd's variables of a valid activation", || {
        systemd(listen("4242", "1", Some("blaze")), OWN, SOCKET)
    });
    assert_eq!(fd, Ok(FIRST_FD));
    let one = blaze_alloc_count::assert_at_most(0, "launchd's one descriptor", || launchd(Launchd::Fds(1), SOCKET));
    assert_eq!(one, Ok(()));
}

#[test]
fn another_process_sockets_are_refused() {
    let what = said(systemd(listen("4243", "1", Some("blaze")), OWN, SOCKET));
    assert!(what.contains("LISTEN_PID=4243") && what.contains("(4242)"), "{what}");
}

#[test]
fn nothing_passed_names_what_is_missing() {
    let what = said(systemd(Listen::default(), OWN, SOCKET));
    assert!(what.contains("LISTEN_PID and LISTEN_FDS not set"), "{what}");
    let what = said(systemd(Listen { pid: Some("4242"), ..Listen::default() }, OWN, SOCKET));
    assert!(what.contains("LISTEN_FDS not set"), "{what}");
    let what = said(systemd(Listen { fds: Some("1"), ..Listen::default() }, OWN, SOCKET));
    assert!(what.contains("LISTEN_PID not set"), "{what}");
    assert!(said(systemd(listen("4242", "0", Some("")), OWN, SOCKET)).contains("LISTEN_FDS=0"));
}

#[test]
fn too_many_sockets_are_refused_with_their_fds() {
    let what = said(systemd(listen("4242", "3", Some("blaze:http:other")), OWN, SOCKET));
    assert!(what.contains("3 sockets (fds 3 to 5)"), "{what}");
    let huge = said(systemd(listen("4242", "9999999999999999999", None), OWN, SOCKET));
    assert!(huge.contains("9999999999999999999 sockets"), "{huge}");
}

#[test]
fn numbers_are_digits_only() {
    for pid in ["", "+4242", " 4242", "4242 ", "-1", "0x10", "42a", "99999999999999999999"] {
        let what = said(systemd(listen(pid, "1", Some("blaze")), OWN, SOCKET));
        assert!(what.contains("is no process ID"), "{pid:?}: {what}");
    }
    for fds in ["", "+1", "one", "1 "] {
        let what = said(systemd(listen("4242", fds, Some("blaze")), OWN, SOCKET));
        assert!(what.contains("is no count"), "{fds:?}: {what}");
    }
}

#[test]
fn names_must_be_one_and_blaze() {
    let what = said(systemd(listen("4242", "1", None), OWN, SOCKET));
    assert!(what.contains("no LISTEN_FDNAMES") && what.contains("FileDescriptorName=blaze"), "{what}");
    let what = said(systemd(listen("4242", "1", Some("other")), OWN, SOCKET));
    assert!(what.contains("named \"other\", not \"blaze\""), "{what}");
    let what = said(systemd(listen("4242", "1", Some("blaze:blaze")), OWN, SOCKET));
    assert!(what.contains("names 2 sockets"), "{what}");
    let what = said(systemd(listen("4242", "1", Some("")), OWN, SOCKET));
    assert!(what.contains("named \"\""), "{what}");
}

#[test]
fn launchd_answers() {
    assert_eq!(launchd(Launchd::Fds(1), SOCKET), Ok(()));
    let cases = [
        (Launchd::Fds(0), "no descriptor"),
        (Launchd::Fds(2), "passed 2 descriptors"),
        (Launchd::NotManaged, "ESRCH"),
        (Launchd::NoSuchSocket, "no socket named blaze"),
        (Launchd::Already, "taken already"),
        (Launchd::Other(13), "launch_activate_socket(blaze) failed"),
    ];
    for (answer, says) in cases {
        let what = launchd(answer, SOCKET).expect_err("refused").to_string();
        assert!(what.starts_with("blaze serve --activated: ") && what.contains(says), "{answer:?}: {what}");
    }
}

#[cfg(unix)]
#[test]
fn passed_owns_its_descriptor() {
    use std::os::{fd::OwnedFd, unix::net::UnixListener};

    use super::{Manager, Passed};

    let dir = std::env::temp_dir().join(format!("blaze-activation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s");
    _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let passed = Passed::new(OwnedFd::from(listener), Manager::Systemd);
    assert!(passed.fd() >= 0);
    assert_eq!(passed.by(), Manager::Systemd);
    assert!(format!("{passed:?}").contains("Systemd"));
    drop(passed);
    assert!(std::os::unix::net::UnixStream::connect(&path).is_err(), "the socket is closed with it");
    std::fs::remove_dir_all(&dir).unwrap();
}
