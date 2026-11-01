//! A registry that requires a token, served from memory: `config.json` asked first with no
//! token and again with it after a 401, the index and the `.crate` files asked with it, and a
//! missing or refused token said as cargo says it; credential processes and
//! `cargo:token-from-stdout` asked as cargo asks them.

use std::{io::Write, path::PathBuf, sync::Arc};

use blaze_hash::HashMap;
use blaze_registry::{
    Context, Locked, Package, SourceConfig, Sources,
    auth::{Auth, Shell},
    download::checksum,
    http::{Http, Response},
};
use blaze_source::SourceId;
use parking_lot::Mutex;
use semver::Version;

const BASE: &str = "https://private.example.com/";
const DL: &str = "https://dl.private.example.com/";
const LOGIN: &str = "https://private.example.com/me";

/// Files by URL, every one of which wants `authorization: <token>`, and what was asked.
struct Server {
    token: String,
    files: Mutex<HashMap<String, Vec<u8>>>,
    asked: Mutex<Vec<(String, Option<String>)>>,
}

impl Http for Server {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        let sent = headers.iter().find(|(k, _)| *k == "authorization").map(|(_, v)| v.to_string());
        self.asked.lock().push((url.to_owned(), sent.clone()));
        if sent.as_deref() != Some(self.token.as_str()) {
            return Ok(Response {
                status: 401,
                headers: vec![(
                    "www-authenticate".to_owned(),
                    format!(r#"Basic realm="x", Cargo login_url="{LOGIN}""#),
                )],
                body: b"who are you".to_vec(),
            });
        }
        match self.files.lock().get(url) {
            Some(body) => Ok(Response { status: 200, headers: vec![], body: body.clone() }),
            None => Ok(Response { status: 404, ..Response::default() }),
        }
    }
}

#[derive(Clone, Default)]
struct Said(Arc<Mutex<Vec<String>>>);

impl Shell for Said {
    fn warn(&self, text: &str) {
        self.0.lock().push(format!("warning: {text}"));
    }
    fn note(&self, text: &str) {
        self.0.lock().push(format!("note: {text}"));
    }
    fn status(&self, word: &str, text: &str) {
        self.0.lock().push(format!("{word} {text}"));
    }
    fn verbose(&self) -> bool {
        false
    }
}

fn tarball(name: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let data = b"[package]\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    builder.append_data(&mut header, format!("{name}-1.0.0/Cargo.toml"), &data[..]).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&builder.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

struct Fixture {
    server: Arc<Server>,
    home: PathBuf,
    said: Said,
    package: Package,
}

impl Fixture {
    /// A registry `private` at [`BASE`] wanting `token`, with `config` as
    /// `$CARGO_HOME/config.toml` and `credentials` as `credentials.toml`.
    fn new(name: &str, token: &str, config: &str, credentials: &str) -> Fixture {
        let home = std::env::temp_dir().join(format!("blaze-auth-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let config = format!("[registries.private]\nindex = \"sparse+{BASE}\"\n{config}");
        std::fs::write(home.join("config.toml"), config).unwrap();
        if !credentials.is_empty() {
            std::fs::write(home.join("credentials.toml"), credentials).unwrap();
        }
        let bytes = tarball("secret");
        let sum = checksum(&bytes);
        let files = HashMap::from_iter([
            (
                format!("{BASE}config.json"),
                format!(r#"{{"dl":"{DL}{{crate}}/{{version}}","auth-required":true}}"#).into_bytes(),
            ),
            (
                format!("{BASE}se/cr/secret"),
                format!(r#"{{"name":"secret","vers":"1.0.0","deps":[],"cksum":"{sum}","features":{{}}}}"#)
                    .into_bytes(),
            ),
            (format!("{DL}secret/1.0.0"), bytes),
        ]);
        let server = Arc::new(Server {
            token: token.to_owned(),
            files: Mutex::new(files),
            asked: Mutex::default(),
        });
        let package =
            Package { name: "secret".to_owned(), version: Version::new(1, 0, 0), checksum: sum };
        Fixture { server, home, said: Said::default(), package }
    }

    fn sources(&self, env: &[(&str, &str)]) -> Sources {
        let mut vars = vec![("CARGO_HOME".to_owned(), self.home.display().to_string())];
        vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let config = blaze_manifest::Config::load(&self.home, vars, &[]).unwrap();
        let mut context = Context::new(self.home.clone(), self.server.clone());
        context.net.retry.quiet = true;
        context.auth = Some(Arc::new(Auth::new(config, Box::new(self.said.clone()))));
        Sources::new(&SourceConfig::default(), Arc::new(context)).unwrap()
    }

    /// The package's checksum read from the index, then the package fetched.
    fn fetch(&self, env: &[(&str, &str)]) -> anyhow::Result<()> {
        let sources = self.sources(env);
        let registry = sources.registry(source())?;
        assert_eq!(registry.checksum("secret", &self.package.version)?, self.package.checksum);
        let locked = Locked { source: source(), package: self.package.clone() };
        sources.fetch(&[locked])?;
        Ok(())
    }

    fn asked(&self) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut *self.server.asked.lock())
    }
}

fn source() -> SourceId {
    blaze_registry::registry_source(&format!("sparse+{BASE}")).unwrap()
}

const PROVIDERS: &str = "\n[registry]\nglobal-credential-providers = [\"cargo:token\"]\n";

#[test]
fn sends_the_token_cargo_sends() {
    let fixture =
        Fixture::new("token", "s3cret", PROVIDERS, "[registries.private]\ntoken = \"s3cret\"\n");
    fixture.fetch(&[]).unwrap();
    let token = Some("s3cret".to_owned());
    assert_eq!(
        fixture.asked(),
        [
            (format!("{BASE}config.json"), None),
            (format!("{BASE}config.json"), token.clone()),
            (format!("{BASE}se/cr/secret"), token.clone()),
            (format!("{DL}secret/1.0.0"), token.clone()),
        ]
    );
    assert!(fixture.home.join("registry/src").read_dir().unwrap().next().is_some());

    // The `config.json` kept says auth is required: the token goes with the first request.
    let sources = fixture.sources(&[]);
    let registry = sources.registry(source()).unwrap();
    registry.invalidate();
    registry.config().unwrap();
    assert_eq!(fixture.asked(), [(format!("{BASE}config.json"), token)]);
    assert!(fixture.said.0.lock().is_empty());
}

#[test]
fn the_token_of_the_environment() {
    let fixture = Fixture::new("env", "from-env", PROVIDERS, "");
    fixture.fetch(&[("CARGO_REGISTRIES_PRIVATE_TOKEN", "from-env")]).unwrap();
    let asked = fixture.asked();
    assert_eq!(asked.last().unwrap().1.as_deref(), Some("from-env"));
}

#[test]
fn a_provider_has_to_be_configured() {
    let credentials = "[registries.private]\ntoken = \"s3cret\"\n";
    let fixture = Fixture::new("unconfigured", "s3cret", "", credentials);
    let err = fixture.fetch(&[]).unwrap_err();
    assert_eq!(
        format!("{:#}", err.root_cause()),
        "authenticated registries require a credential-provider to be configured\n\
         see https://doc.rust-lang.org/nightly/cargo/reference/registry-authentication.html for details"
    );
}

#[test]
fn says_what_cargo_says_of_no_token_or_a_refused_one() {
    let fixture = Fixture::new("missing", "s3cret", PROVIDERS, "");
    let err = fixture.fetch(&[]).unwrap_err();
    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
    assert!(
        chain.contains(
            &"no token found for `private`, please run `cargo login --registry private`\n\
              or use environment variable CARGO_REGISTRIES_PRIVATE_TOKEN"
                .to_owned()
        ),
        "{chain:#?}"
    );

    let credentials = "[registries.private]\ntoken = \"wrong\"\n";
    let fixture = Fixture::new("rejected", "s3cret", PROVIDERS, credentials);
    let err = fixture.fetch(&[]).unwrap_err();
    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
    let rejected = chain.iter().position(|it| {
        it == "token rejected for `private`, please run `cargo login --registry private`\n\
               or use environment variable CARGO_REGISTRIES_PRIVATE_TOKEN\n\
               note: the token does not include an authentication scheme"
    });
    let rejected = rejected.unwrap_or_else(|| panic!("{chain:#?}"));
    assert_eq!(
        chain[rejected + 1],
        format!(
            "failed to get successful HTTP response from `{BASE}config.json`, got 401\n\
             body:\nwho are you"
        )
    );
    assert_eq!(fixture.asked().len(), 2, "a 401 is not tried again");
}

#[test]
fn a_token_the_registry_ignores_is_warned_of() {
    let config = "\n[registry]\nglobal-credential-providers = [\"cargo:token-from-stdout x\"]\n";
    let credentials = "[registries.private]\ntoken = \"s3cret\"\n";
    let fixture = Fixture::new("ignored", "s3cret", config, credentials);
    let err = fixture.fetch(&[]).unwrap_err();
    let said = fixture.said.0.lock().clone();
    assert_eq!(
        said,
        [format!(
            "warning: registry `private` has a token configured in {} that will be ignored \
             because the `cargo:token` credential provider is not listed in \
             `registry.global-credential-providers`",
            fixture.home.join("credentials.toml").display()
        )]
    );
    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
    let failed = chain
        .iter()
        .position(|it| it == "credential provider `cargo:token-from-stdout x` failed action `get`");
    let failed = failed.unwrap_or_else(|| panic!("{chain:#?}"));
    assert_eq!(chain[failed + 1], "failed to spawn credential process");
}

/// An executable `name` in the fixture's home.
#[cfg(unix)]
fn script(fixture: &Fixture, name: &str, text: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture.home.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{text}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

/// A credential process that keeps each request in `asked` and gives `answer`.
#[cfg(unix)]
fn credential_process(fixture: &Fixture, answer: &str) -> String {
    script(
        fixture,
        "provider",
        &format!(
            "echo '{{\"v\":[1]}}'\nread -r request\nprintf '%s\\n' \"$request\" >> \"$(dirname \
             \"$0\")/asked\"\necho '{answer}'\ncat > /dev/null\n"
        ),
    )
}

#[cfg(unix)]
fn with_provider(fixture: &Fixture, provider: &str) {
    let config = format!(
        "[registries.private]\nindex = \"sparse+{BASE}\"\n\n[registry]\n\
         global-credential-providers = [\"{provider}\"]\n"
    );
    std::fs::write(fixture.home.join("config.toml"), config).unwrap();
}

#[cfg(unix)]
#[test]
fn a_credential_process_is_asked_as_cargo_asks() {
    let fixture = Fixture::new("process", "s3cret", "", "");
    let answer =
        r#"{"Ok":{"kind":"get","token":"s3cret","cache":"session","operation_independent":true}}"#;
    let provider = credential_process(&fixture, answer);
    with_provider(&fixture, &format!("{provider} an-arg"));
    fixture.fetch(&[]).unwrap();
    let token = Some("s3cret".to_owned());
    assert_eq!(fixture.asked().last().unwrap(), &(format!("{DL}secret/1.0.0"), token));
    let asked = std::fs::read_to_string(fixture.home.join("asked")).unwrap();
    let asked: Vec<serde_json::Value> =
        asked.lines().map(|it| serde_json::from_str(it).unwrap()).collect();
    assert_eq!(asked.len(), 1, "a token of the session is asked for once: {asked:#?}");
    assert_eq!(
        asked[0],
        serde_json::json!({
            "v": 1,
            "registry": {
                "index-url": format!("sparse+{BASE}"),
                "name": "private",
                "headers": [
                    format!(r#"www-authenticate: Basic realm="x", Cargo login_url="{LOGIN}""#)
                ],
            },
            "kind": "get",
            "operation": "read",
            "args": ["an-arg"],
        })
    );
}

#[cfg(unix)]
#[test]
fn a_credential_process_that_fails_is_said_as_cargo_says() {
    let fixture = Fixture::new("process-fails", "s3cret", "", "");
    let answer =
        r#"{"Err":{"kind":"other","message":"the vault is locked","caused-by":["no key","none"]}}"#;
    let provider = credential_process(&fixture, answer);
    with_provider(&fixture, &provider);
    let err = fixture.fetch(&[]).unwrap_err();
    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
    let at = chain
        .iter()
        .position(|it| it == &format!("credential provider `{provider}` failed action `get`"));
    let at = at.unwrap_or_else(|| panic!("{chain:#?}"));
    assert_eq!(chain[at + 1..], ["the vault is locked", "no key", "none"]);

    let fixture = Fixture::new("process-not-found", "s3cret", "", "");
    let provider = credential_process(&fixture, r#"{"Err":{"kind":"not-found"}}"#);
    with_provider(&fixture, &provider);
    let err = fixture.fetch(&[]).unwrap_err();
    assert!(
        err.chain().any(|it| it.to_string().starts_with("no token found for `private`")),
        "{err:#}"
    );
}

#[cfg(unix)]
#[test]
fn token_from_stdout_runs_its_command() {
    let fixture = Fixture::new("stdout", "s3cret", "", "");
    let print = script(
        &fixture,
        "print",
        "echo \"$1 $CARGO_REGISTRY_NAME_OPT $CARGO_REGISTRY_INDEX_URL\" > \"$(dirname \"$0\")/asked\"\n\
         printf 's3cret\\r\\n'\n",
    );
    with_provider(&fixture, &format!("cargo:token-from-stdout {print} {{index_url}}"));
    fixture.fetch(&[]).unwrap();
    assert_eq!(fixture.asked().last().unwrap().1.as_deref(), Some("s3cret"));
    assert_eq!(
        std::fs::read_to_string(fixture.home.join("asked")).unwrap(),
        format!("sparse+{BASE} private sparse+{BASE}\n")
    );

    let fixture = Fixture::new("stdout-lines", "s3cret", "", "");
    let print = script(&fixture, "print", "echo one\necho two\n");
    with_provider(&fixture, &format!("cargo:token-from-stdout {print}"));
    let err = fixture.fetch(&[]).unwrap_err();
    let said =
        format!("process `{print}` returned more than one line of output; expected a single token");
    assert!(err.chain().any(|it| it.to_string() == said), "{err:#}");
}

#[test]
fn a_provider_blaze_does_not_run_is_named() {
    let provider = if cfg!(target_os = "macos") {
        "cargo:macos-keychain"
    } else if cfg!(windows) {
        "cargo:wincred"
    } else {
        "cargo:libsecret"
    };
    let config = format!("\n[registry]\nglobal-credential-providers = [\"{provider}\"]\n");
    let fixture = Fixture::new("not-run", "s3cret", &config, "");
    let err = fixture.fetch(&[]).unwrap_err();
    assert!(
        err.chain().any(|it| it.to_string()
            == format!(
                "credential provider `{provider}` is not one blaze runs yet: blaze runs \
                 `cargo:token`, `cargo:token-from-stdout` and credential processes\nname one \
                 of those for registry `private` in `registry.global-credential-providers` \
                 or its `credential-provider`"
            )),
        "{err:#}"
    );
}
