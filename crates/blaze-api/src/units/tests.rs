use std::path::{Path, PathBuf};

use super::{
    Installed, LABEL, PLIST_FILE, Place, Step, Unit, UnitFile, captured, files, install, installed, main_file, socket_of,
    uninstall, unit_dir,
};
use crate::activation::Manager;

fn mac() -> (Unit, Place) {
    let unit = Unit {
        program: "/opt/blaze/bin/blaze".into(),
        socket: "/Users/me/.cache/blaze/v2.sock".into(),
        log: "/Users/me/.cache/blaze/v2/serve.log".into(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into()), ("HOME".into(), "/Users/me".into())],
    };
    (unit, Place { manager: Manager::Launchd, dir: "/Users/me/Library/LaunchAgents".into(), uid: 501 })
}

fn linux() -> (Unit, Place) {
    let unit = Unit {
        program: "/home/me/my tools/blaze".into(),
        socket: "/run/user/1000/blaze/v2.sock".into(),
        log: "/home/me/.cache/blaze/v2/serve.log".into(),
        env: vec![("PATH".into(), "/usr/bin:/home/me/100%/bin".into()), ("HOME".into(), "/home/me".into())],
    };
    (unit, Place { manager: Manager::Systemd, dir: "/home/me/.config/systemd/user".into(), uid: 1000 })
}

const PLIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<!-- Written by `blaze daemon install`; `blaze daemon uninstall` removes it. -->\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\
\t<string>inc.indexable.blaze</string>\n\
\t<key>ProgramArguments</key>\n\
\t<array>\n\
\t\t<string>/opt/blaze/bin/blaze</string>\n\
\t\t<string>serve</string>\n\
\t\t<string>--activated</string>\n\
\t\t<string>--socket</string>\n\
\t\t<string>/Users/me/.cache/blaze/v2.sock</string>\n\
\t</array>\n\
\t<key>EnvironmentVariables</key>\n\
\t<dict>\n\
\t\t<key>HOME</key>\n\
\t\t<string>/Users/me</string>\n\
\t\t<key>PATH</key>\n\
\t\t<string>/usr/bin:/bin</string>\n\
\t</dict>\n\
\t<key>Sockets</key>\n\
\t<dict>\n\
\t\t<key>blaze</key>\n\
\t\t<dict>\n\
\t\t\t<key>SockPathName</key>\n\
\t\t\t<string>/Users/me/.cache/blaze/v2.sock</string>\n\
\t\t\t<key>SockPathMode</key>\n\
\t\t\t<integer>384</integer>\n\
\t\t</dict>\n\
\t</dict>\n\
\t<key>RunAtLoad</key>\n\
\t<false/>\n\
\t<key>ProcessType</key>\n\
\t<string>Interactive</string>\n\
\t<key>ThrottleInterval</key>\n\
\t<integer>1</integer>\n\
\t<key>StandardOutPath</key>\n\
\t<string>/Users/me/.cache/blaze/v2/serve.log</string>\n\
\t<key>StandardErrorPath</key>\n\
\t<string>/Users/me/.cache/blaze/v2/serve.log</string>\n\
</dict>\n\
</plist>\n";

const SOCKET_UNIT: &str = "# Written by `blaze daemon install`; `blaze daemon uninstall` removes it.\n\
[Unit]\n\
Description=blaze's daemon socket\n\
\n\
[Socket]\n\
ListenStream=/run/user/1000/blaze/v2.sock\n\
FileDescriptorName=blaze\n\
SocketMode=0600\n\
DirectoryMode=0700\n\
RemoveOnStop=yes\n\
\n\
[Install]\n\
WantedBy=sockets.target\n";

const SERVICE_UNIT: &str = "# Written by `blaze daemon install`; `blaze daemon uninstall` removes it.\n\
[Unit]\n\
Description=blaze's daemon (blaze serve), started by blaze.socket\n\
Requires=blaze.socket\n\
After=blaze.socket\n\
StartLimitIntervalSec=0\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart=\"/home/me/my tools/blaze\" serve --activated --socket /run/user/1000/blaze/v2.sock\n\
Environment=\"HOME=/home/me\"\n\
Environment=\"PATH=/usr/bin:/home/me/100%%/bin\"\n\
StandardOutput=append:/home/me/.cache/blaze/v2/serve.log\n\
StandardError=append:/home/me/.cache/blaze/v2/serve.log\n\
KillMode=mixed\n";

#[test]
fn launchd_agent_golden() {
    let (unit, place) = mac();
    let made = files(&unit, &place).unwrap();
    assert_eq!(made, vec![UnitFile { path: "/Users/me/Library/LaunchAgents/inc.indexable.blaze.plist".into(), text: PLIST.into() }]);
}

#[test]
fn systemd_units_golden() {
    let (unit, place) = linux();
    let made = files(&unit, &place).unwrap();
    assert_eq!(made.len(), 2);
    assert_eq!(made[0].path, Path::new("/home/me/.config/systemd/user/blaze.socket"));
    assert_eq!(made[0].text, SOCKET_UNIT);
    assert_eq!(made[1].path, Path::new("/home/me/.config/systemd/user/blaze.service"));
    assert_eq!(made[1].text, SERVICE_UNIT);
}

#[test]
fn special_text_is_escaped() {
    let (mut unit, place) = mac();
    unit.socket = "/Users/a&b <c>/v2.sock".into();
    unit.env = vec![("BLAZE_HOME".into(), "/Users/\"q\" 'x'".into())];
    let text = &files(&unit, &place).unwrap()[0].text;
    assert!(text.contains("<string>/Users/a&amp;b &lt;c&gt;/v2.sock</string>"), "{text}");
    assert!(text.contains("<string>/Users/&quot;q&quot; &apos;x&apos;</string>"), "{text}");
    assert_eq!(socket_of(Manager::Launchd, text), Some(PathBuf::from("/Users/a&b <c>/v2.sock")));

    let (mut unit, place) = linux();
    unit.program = "/opt/a\"b\\c$d;".into();
    unit.socket = "/run/100%/v2.sock".into();
    unit.env = vec![("BLAZE_HOME".into(), "/home/\"q\"\\$x".into())];
    let made = files(&unit, &place).unwrap();
    assert!(made[0].text.contains("\nListenStream=/run/100%%/v2.sock\n"), "{}", made[0].text);
    assert_eq!(socket_of(Manager::Systemd, &made[0].text), Some(PathBuf::from("/run/100%/v2.sock")));
    let service = &made[1].text;
    assert!(service.contains("\nExecStart=\"/opt/a\\\"b\\\\c$$d;\" serve --activated --socket /run/100%%/v2.sock\n"), "{service}");
    assert!(service.contains("\nEnvironment=\"BLAZE_HOME=/home/\\\"q\\\"\\\\$x\"\n"), "{service}");
}

#[test]
fn a_plain_word_doubles_its_specifiers_and_dollars() {
    let (mut unit, place) = linux();
    unit.program = "/opt/a$b%c".into();
    unit.log = "/var/log/100%".into();
    unit.env = vec![("PATH".into(), "/a%$b".into())];
    let service = &files(&unit, &place).unwrap()[1].text;
    assert!(service.contains("\nExecStart=/opt/a$$b%%c serve --activated --socket "), "{service}");
    assert!(service.contains("\nEnvironment=\"PATH=/a%%$b\"\n"), "{service}");
    assert!(service.contains("\nStandardOutput=append:/var/log/100%%\nStandardError=append:/var/log/100%%\n"), "{service}");
    assert_eq!(PLIST_FILE, format!("{LABEL}.plist"));
}

#[test]
fn what_no_unit_can_hold_is_refused() {
    let refused = |unit: &Unit, place: &Place| files(unit, place).expect_err("refused").what;
    let (unit, place) = mac();

    let relative = Unit { socket: "v2.sock".into(), ..unit.clone() };
    assert!(refused(&relative, &place).contains("v2.sock is not an absolute path"));
    let control = Unit { log: "/tmp/a\nb".into(), ..unit.clone() };
    assert!(refused(&control, &place).contains("control character"));
    let long = Unit { socket: PathBuf::from(format!("/{}", "s".repeat(103))), ..unit.clone() };
    assert!(refused(&long, &place).contains("104 bytes long") && refused(&long, &place).contains("at most 103"));
    let fits = Unit { socket: PathBuf::from(format!("/{}", "s".repeat(102))), ..unit.clone() };
    assert!(files(&fits, &place).is_ok());
    let twice = Unit { env: vec![("PATH".into(), "/a".into()), ("PATH".into(), "/b".into())], ..unit.clone() };
    assert!(refused(&twice, &place).contains("PATH is given twice"));
    let unnamed = Unit { env: vec![("1PATH".into(), "/a".into())], ..unit.clone() };
    assert!(refused(&unnamed, &place).contains("\"1PATH\" is not a variable's name"));
    let tab = Unit { env: vec![("PATH".into(), "/a\tb".into())], ..unit.clone() };
    assert!(refused(&tab, &place).contains("control character"));
    let nowhere = Place { dir: "LaunchAgents".into(), ..place.clone() };
    assert!(refused(&unit, &nowhere).contains("is not an absolute path"));

    let (unit, place) = linux();
    let spaced = Unit { socket: "/run/a b/v2.sock".into(), ..unit.clone() };
    assert!(refused(&spaced, &place).contains("which a systemd unit cannot name it with"));
    let slashed = Unit { log: "/tmp/a\\b".into(), ..unit.clone() };
    assert!(refused(&slashed, &place).contains("'\\\\'"));
    let long = Unit { socket: PathBuf::from(format!("/{}", "s".repeat(107))), ..unit.clone() };
    assert!(refused(&long, &place).contains("at most 107"));
}

#[test]
fn install_and_uninstall_steps() {
    let run = |argv: &[&str]| Step::Run(argv.iter().map(|it| (*it).to_owned()).collect());
    let words = |argv: &[&str]| argv.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();

    let (unit, place) = mac();
    let plist = main_file(&place);
    let service = format!("gui/501/{LABEL}");
    let bootout = Step::Undo {
        argv: words(&["launchctl", "bootout", &service]),
        loaded: words(&["launchctl", "print", &service]),
    };
    assert_eq!(install(&unit, &place).unwrap(), vec![
        Step::MakeDir("/Users/me/Library/LaunchAgents".into()),
        Step::MakeDir("/Users/me/.cache/blaze".into()),
        Step::MakeDir("/Users/me/.cache/blaze/v2".into()),
        bootout.clone(),
        Step::Write(UnitFile { path: plist.clone(), text: PLIST.into() }),
        run(&["launchctl", "enable", &service]),
        run(&["launchctl", "bootstrap", "gui/501", plist.to_str().unwrap()]),
    ]);
    assert_eq!(uninstall(&place, &unit.socket), vec![
        bootout,
        Step::Remove(plist),
        Step::Remove(unit.socket.clone()),
    ]);

    let (unit, place) = linux();
    let steps = install(&unit, &place).unwrap();
    assert_eq!(steps[..3], [
        Step::MakeDir("/home/me/.config/systemd/user".into()),
        Step::MakeDir("/run/user/1000/blaze".into()),
        Step::MakeDir("/home/me/.cache/blaze/v2".into()),
    ]);
    assert!(matches!(&steps[3], Step::Write(file) if file.text == SOCKET_UNIT));
    assert!(matches!(&steps[4], Step::Write(file) if file.text == SERVICE_UNIT));
    assert_eq!(steps[5..], [
        run(&["systemctl", "--user", "daemon-reload"]),
        run(&["systemctl", "--user", "enable", "blaze.socket"]),
        run(&["systemctl", "--user", "restart", "blaze.socket"]),
    ]);
    assert_eq!(uninstall(&place, &unit.socket), vec![
        run(&["systemctl", "--user", "disable", "--now", "blaze.socket"]),
        run(&["systemctl", "--user", "stop", "blaze.service"]),
        Step::Remove("/home/me/.config/systemd/user/blaze.socket".into()),
        Step::Remove("/home/me/.config/systemd/user/blaze.service".into()),
        run(&["systemctl", "--user", "daemon-reload"]),
        Step::Remove(unit.socket.clone()),
    ]);
    assert_eq!(steps[5].to_string(), "running `systemctl --user daemon-reload`");
}

#[test]
fn installed_is_read_from_the_main_file() {
    let dir = std::env::temp_dir().join(format!("blaze-units-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for manager in [Manager::Launchd, Manager::Systemd] {
        let (unit, _) = if manager == Manager::Launchd { mac() } else { linux() };
        let place = Place { manager, dir: dir.clone(), uid: 501 };
        assert_eq!(installed(&place, &unit.socket), Ok(Installed::No));
        for file in files(&unit, &place).unwrap() {
            std::fs::write(&file.path, &file.text).unwrap();
        }
        assert_eq!(installed(&place, &unit.socket), Ok(Installed::Yes));
        assert_eq!(installed(&place, Path::new("/elsewhere.sock")), Ok(Installed::Elsewhere(unit.socket.clone())));
        std::fs::write(main_file(&place), "garbage").unwrap();
        let error = installed(&place, &unit.socket).expect_err("a main file of no socket");
        assert!(error.what.contains("names no socket"), "{error}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn captured_variables_and_unit_dirs() {
    let env = captured(|name| match name {
        "PATH" => Some("/bin".to_owned()),
        "HOME" => Some("/h".to_owned()),
        "BLAZE_HOME" => Some(String::new()),
        "BLAZE_SANDBOX" => Some("off".to_owned()),
        _ => None,
    });
    assert_eq!(env, vec![("HOME".to_owned(), "/h".to_owned()), ("PATH".to_owned(), "/bin".to_owned())]);

    let home = Path::new("/h");
    assert_eq!(unit_dir(Manager::Launchd, home, None), Path::new("/h/Library/LaunchAgents"));
    assert_eq!(unit_dir(Manager::Systemd, home, None), Path::new("/h/.config/systemd/user"));
    assert_eq!(unit_dir(Manager::Systemd, home, Some(Path::new("/c"))), Path::new("/c/systemd/user"));
    assert_eq!(unit_dir(Manager::Systemd, home, Some(Path::new("rel"))), Path::new("/h/.config/systemd/user"));
}

#[cfg(unix)]
#[test]
fn steps_run_in_order_and_the_first_failure_is_named() {
    use std::os::unix::fs::PermissionsExt as _;

    use super::run;

    let words = |argv: &[&str]| argv.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    let dir = std::env::temp_dir().join(format!("blaze-units-run-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    let file = dir.join("a/b/unit");
    run(&[
        Step::MakeDir(dir.join("a/b")),
        Step::Write(UnitFile { path: file.clone(), text: "one".into() }),
        Step::Write(UnitFile { path: file.clone(), text: "two".into() }),
        Step::Run(words(&["true"])),
        Step::Undo { argv: words(&["false"]), loaded: words(&["false"]) },
        Step::Remove(dir.join("never-there")),
    ])
    .unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(dir.join("a")).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::read_dir(dir.join("a/b")).unwrap().count(), 1, "no temporary is left");

    let still = run(&[Step::Undo { argv: words(&["false"]), loaded: words(&["true"]) }]).unwrap_err();
    assert!(still.what.contains("running `false`") && still.what.contains("still loaded"), "{still}");
    let failed = run(&[Step::Run(words(&["false"])), Step::Remove(file.clone())]).unwrap_err();
    assert!(failed.what.starts_with("running `false`: "), "{failed}");
    assert!(file.exists(), "nothing after the failed step ran");
    let missing = run(&[Step::Run(words(&["/no/such/program"]))]).unwrap_err();
    assert!(missing.what.contains("did not start"), "{missing}");
    run(&[Step::Remove(file.clone())]).unwrap();
    assert!(!file.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
