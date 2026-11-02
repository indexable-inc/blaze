use super::*;

const EXAMPLE: &str = r#"{
    "repo": "indexable-inc/blaze",
    "app": {
        "id": 123,
        "installation": 456,
        "key_file": "/etc/blaze-ci/app.pem",
        "api": "api.github.com"
    },
    "webhook": { "listen": "127.0.0.1:8080", "secret_file": "/etc/blaze-ci/webhook" },
    "daemon": { "socket": "/run/blaze/api.sock" },
    "git": { "dir": "/var/lib/blaze-ci/repo", "remote": "https://github.com/indexable-inc/blaze" },
    "default_branch": "main",
    "checks": [
        { "name": "blaze/check", "kind": "check", "targets": { "profile": "dev" } },
        {
            "name": "blaze/test",
            "kind": "test",
            "targets": { "packages": ["blaze-ci"], "profile": "dev", "features": ["x"] },
            "filter": ["service"]
        }
    ],
    "release_checks": [
        {
            "name": "blaze/release",
            "kind": "build",
            "targets": { "profile": "release", "target": "x86_64-unknown-linux-gnu" }
        }
    ],
    "oracle": { "one_in": 50, "targets": { "profile": "dev" } },
    "snapshot": {
        "every_secs": 3600,
        "command": ["ix", "snapshot", "{name}"],
        "remove": ["ix", "snapshot", "rm", "{id}"],
        "keep": 3
    },
    "standby": {
        "primary": "10.0.0.2:7070",
        "token_file": "/etc/blaze-ci/token",
        "every_secs": 10,
        "misses": 6,
        "latest": ["ix", "snapshot", "latest"],
        "restore": ["ix", "restore", "{id}"],
        "promote": ["ix", "promote"]
    },
    "state": "/var/lib/blaze-ci/state.json"
}"#;

#[test]
fn a_full_example_parses() {
    let config = Config::parse(EXAMPLE).unwrap();
    assert_eq!(config.repo, "indexable-inc/blaze");
    assert_eq!(config.app.installation, 456);
    assert_eq!(config.webhook.listen, "127.0.0.1:8080".parse().unwrap());
    assert_eq!(config.checks.len(), 2);
    assert_eq!(config.checks[1].kind, CheckKind::Test);
    assert_eq!(config.checks[1].filter, ["service"]);
    assert_eq!(config.release_checks[0].targets.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
    assert_eq!(config.oracle.one_in, 50);
    assert_eq!(config.snapshot.keep, 3);
    assert_eq!(config.standby.misses, 6);
    let request = config.checks[1].request(ViewRef::Head("h".to_owned()));
    let Request::Test { targets, filter, all, .. } = request else { panic!("{request:?}") };
    assert_eq!(targets.packages, ["blaze-ci"]);
    assert_eq!(filter, ["service"]);
    assert!(!all);
}

#[test]
fn an_unknown_field_is_refused_naming_it() {
    let text = EXAMPLE.replace("\"keep\": 3", "\"keep\": 3, \"kept\": 4");
    let error = Config::parse(&text).unwrap_err();
    assert!(error.what.contains("kept"), "{error}");
    let text = EXAMPLE.replace("\"api\": \"api.github.com\"", "\"api\": \"x\", \"token\": \"t\"");
    let error = Config::parse(&text).unwrap_err();
    assert!(error.what.contains("token"), "{error}");
}

#[test]
fn a_filter_on_a_check_that_is_not_a_test_is_refused() {
    let text = EXAMPLE.replace(
        r#""kind": "check", "targets": { "profile": "dev" } }"#,
        r#""kind": "check", "targets": { "profile": "dev" }, "all": true }"#,
    );
    let error = Config::parse(&text).unwrap_err();
    assert!(error.what.contains("blaze/check"), "{error}");
}

#[test]
fn the_oracles_name_is_refused_for_a_check() {
    let text = EXAMPLE.replace("blaze/release", oracle::CHECK);
    assert!(Config::parse(&text).unwrap_err().what.contains(oracle::CHECK));
}

#[test]
fn a_secret_hides_in_debug_and_loses_its_newline() {
    let path = std::env::temp_dir().join(format!("blaze-ci-secret-{}", std::process::id()));
    std::fs::write(&path, "hunter2\n").unwrap();
    let secret = Secret::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(secret.expose(), b"hunter2");
    assert!(!format!("{secret:?}").contains("hunter2"));
}
