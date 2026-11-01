//! A registry's token as cargo 1.101 finds and sends it (`src/util/auth/mod.rs`,
//! `src/util/credential/token.rs`): the providers of `registry.global-credential-providers`, or
//! the registry's own `credential-provider`, asked in turn; `cargo:token` reading
//! `credentials.toml` and `CARGO_REGISTRIES_<NAME>_TOKEN`, `cargo:token-from-stdout` and
//! credential processes run ([`process`]); a token kept for the rest of the run as its provider
//! says; and cargo's words when there is none or the registry refuses it.

mod process;
mod protocol;

use std::{
    fmt,
    time::{Duration, SystemTime},
};

use anyhow::bail;
use blaze_hash::HashMap;
use blaze_manifest::config::{Config, OptValue, PathAndArgs, Value, env_key};
use blaze_manifest::files::Disk;
use blaze_source::SourceId;
use parking_lot::Mutex;
use serde_derive::Serialize;

use crate::{api::check_token, registry_source};

/// A registry as cargo's `SourceId` names it: its source, and its key, `crates-io` or the
/// `[registries]` name it was asked by, when it has one.
#[derive(Clone, Copy)]
pub struct Named<'a> {
    pub source: &'a SourceId,
    pub name: Option<&'a str>,
}

impl Named<'_> {
    /// cargo's `display_registry_name`.
    pub fn shown(&self) -> String {
        match self.name {
            Some(name) => name.to_owned(),
            None => self.source.url().to_string(),
        }
    }

    /// cargo's `Display` of a registry's `SourceId`.
    pub fn described(&self) -> String {
        format!("registry `{}`", self.shown())
    }
}

/// Where what cargo's shell says of credentials goes.
pub trait Shell: Send + Sync {
    fn warn(&self, text: &str);
    fn note(&self, text: &str);
    fn status(&self, word: &str, text: &str);
    /// Whether each provider is named as it is asked (`--verbose`).
    fn verbose(&self) -> bool;
}

/// Plain lines on standard error, for a caller with no shell of its own.
pub struct Stderr;

impl Shell for Stderr {
    fn warn(&self, text: &str) {
        eprintln!("warning: {text}");
    }

    fn note(&self, text: &str) {
        eprintln!("note: {text}");
    }

    fn status(&self, word: &str, text: &str) {
        eprintln!("{word:>12} {text}");
    }

    fn verbose(&self) -> bool {
        false
    }
}

/// What a token is asked for, as `cargo_credential::Operation`.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum Operation<'a> {
    /// Reading the index or fetching a `.crate`; a search.
    Read,
    Publish {
        name: &'a str,
        vers: &'a str,
        cksum: &'a str,
    },
    Yank {
        name: &'a str,
        vers: &'a str,
    },
    Unyank {
        name: &'a str,
        vers: &'a str,
    },
    Owners {
        name: &'a str,
    },
}

/// What the providers are asked to do, as `cargo_credential::Action`.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Action<'a> {
    Get(Operation<'a>),
    Login {
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<&'a str>,
        #[serde(rename = "login-url", skip_serializing_if = "Option::is_none")]
        login_url: Option<&'a str>,
    },
    Logout,
}

impl fmt::Display for Action<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::Get(_) => "get",
            Action::Login { .. } => "login",
            Action::Logout => "logout",
        })
    }
}

/// What a provider answered, as `cargo_credential::CredentialResponse`.
#[derive(Debug)]
enum Response {
    Get {
        token: String,
        cache: CacheControl,
        operation_independent: bool,
    },
    Login,
    Logout,
    /// A kind this protocol version does not have.
    Unknown,
}

/// How long a token may be used again in this run.
#[derive(Debug)]
enum CacheControl {
    Never,
    Expires(SystemTime),
    Session,
}

/// A provider that did not answer: the two cargo then asks the next provider after, or one
/// that ends the asking.
enum Failure {
    UrlNotSupported,
    NotFound,
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Failure {
        Failure::Fatal(e)
    }
}

impl Failure {
    fn operation_not_supported() -> Failure {
        Failure::Fatal(anyhow::anyhow!("requested operation not supported"))
    }
}

/// No provider had a token, or one to remove: `cargo_credential::Error::NotFound`.
#[derive(Debug)]
struct NotFound;

impl fmt::Display for NotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("credential not found")
    }
}

impl std::error::Error for NotFound {}

/// cargo's built-in providers, which an alias cannot shadow.
const BUILT_IN: &[&str] =
    &[TOKEN, "cargo:token-from-stdout", "cargo:wincred", "cargo:macos-keychain", "cargo:libsecret"];

const TOKEN: &str = "cargo:token";

/// The built-in provider for this platform that cargo runs and blaze does not yet.
fn not_run_here(name: &str) -> bool {
    match name {
        "cargo:wincred" => cfg!(windows),
        "cargo:macos-keychain" => cfg!(target_os = "macos"),
        "cargo:libsecret" => cfg!(target_os = "linux"),
        _ => false,
    }
}

/// A registry's `[registry]` (crates.io) or `[registries.<name>]` credentials: its token
/// ([`Config::credentials_token`]) and the provider the configuration names for it.
#[derive(Clone)]
struct Credentials {
    token: OptValue<String>,
    provider: Option<PathAndArgs>,
}

/// A token kept for the rest of the run, cargo's `CredentialCacheValue`.
struct Cached {
    token: String,
    expiration: Option<SystemTime>,
    operation_independent: bool,
}

/// Why a registry wants a token it does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    TokenMissing,
    TokenRejected,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reason::TokenMissing => "no token found",
            Reason::TokenRejected => "token rejected",
        })
    }
}

/// A registry that wants a token there is none of, or that refused the one sent, in cargo's
/// words (`AuthorizationError`).
#[derive(Debug)]
pub struct AuthorizationError {
    crates_io: bool,
    name: Option<String>,
    url: String,
    default_registry: bool,
    pub login_url: Option<String>,
    reason: Reason,
    /// Whether `cargo:token` is among the providers, so that `cargo login` would help.
    cargo_token: bool,
    /// Whether the token kept for the registry has no scheme (no space in it).
    token_lacks_scheme: Option<bool>,
}

impl std::error::Error for AuthorizationError {}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.crates_io {
            let args = if self.default_registry { " --registry crates-io" } else { "" };
            write!(f, "{}, please run `cargo login{args}`", self.reason)?;
            if self.cargo_token {
                write!(f, "\nor use environment variable CARGO_REGISTRY_TOKEN")?;
            }
            Ok(())
        } else if let Some(name) = &self.name {
            write!(f, "{} for `{name}`", self.reason)?;
            match self.cargo_token {
                true => write!(
                    f,
                    ", please run `cargo login --registry {name}`\nor use environment variable {}",
                    env_key(&format!("registries.{name}.token"))
                )?,
                false => {
                    write!(f, "\nYou may need to log in using this registry's credential provider")?
                }
            }
            if self.reason == Reason::TokenRejected && self.token_lacks_scheme == Some(true) {
                write!(f, "\nnote: the token does not include an authentication scheme")?;
            }
            Ok(())
        } else if self.reason == Reason::TokenMissing {
            write!(
                f,
                "{} for `{url}`\nconsider setting up an alternate registry in Cargo's \
                 configuration\nas described by \
                 https://doc.rust-lang.org/cargo/reference/registries.html\n\n[registries]\n\
                 my-registry = {{ index = \"{url}\" }}\n",
                self.reason,
                url = self.url
            )
        } else {
            write!(f, "{} for `{}`", self.reason, self.url)
        }
    }
}

/// The credentials of one run: its configuration, the tokens its providers gave, and the
/// registries' credentials as read, behind one lock, so that fetches at once ask a provider
/// once, as cargo, which asks from one thread.
pub struct Auth {
    state: Mutex<State>,
    // dyn: the CLI's output, written to only while a token is found, once per registry per run.
    shell: Box<dyn Shell>,
}

struct State {
    config: Config<Disk>,
    /// By the registry's canonical URL.
    tokens: HashMap<String, Cached>,
    /// By the registry's canonical URL, cargo's `registry_config` cache.
    registries: HashMap<String, Option<Credentials>>,
}

impl Auth {
    // dyn: the CLI's output, as on `Auth::shell`.
    pub fn new(config: Config<Disk>, shell: Box<dyn Shell>) -> Auth {
        let state = State { config, tokens: HashMap::default(), registries: HashMap::default() };
        Auth { state: Mutex::new(state), shell }
    }

    fn state(&self) -> parking_lot::MutexGuard<'_, State> {
        self.state.lock()
    }

    /// The name a registry is configured by, found by its index URL as cargo finds it for
    /// credentials: `crates-io` for crates.io.
    pub fn name_of(&self, source: &SourceId) -> anyhow::Result<Option<String>> {
        if source.is_crates_io() {
            return Ok(Some(blaze_source::CRATES_IO_REGISTRY.to_owned()));
        }
        registry_name(&self.state().config, source)
    }

    /// Keeps `--token`'s token for the registry for the rest of the run, for any operation
    /// (cargo's `cache_token_from_commandline`).
    pub fn keep_command_line_token(&self, source: &SourceId, token: &str) {
        let cached =
            Cached { token: token.to_owned(), expiration: None, operation_independent: true };
        self.state()
            .tokens
            .insert(source.canonical_url().raw_canonicalized_url().to_string(), cached);
    }

    /// The token to send to the registry for `operation` (cargo's `auth_token`), with the
    /// headers of a 401 it answered, if any. `require_provider` is the asking of an index or a
    /// download, which with no `registry.global-credential-providers` set is an error.
    pub fn token(
        &self,
        registry: Named<'_>,
        login_url: Option<&str>,
        operation: Operation<'_>,
        headers: &[String],
        require_provider: bool,
    ) -> anyhow::Result<String> {
        let mut state = self.state();
        match state.token_optional(&*self.shell, registry, operation, headers, require_provider)? {
            Some(token) => Ok(token),
            None => Err(state
                .authorization_error(&*self.shell, registry, login_url, Reason::TokenMissing)?
                .into()),
        }
    }

    /// The error of a registry that refused the token sent.
    pub fn rejected(
        &self,
        registry: Named<'_>,
        login_url: Option<&str>,
    ) -> anyhow::Result<AuthorizationError> {
        self.state().authorization_error(&*self.shell, registry, login_url, Reason::TokenRejected)
    }

    /// The error of a registry that wants a token there is none of.
    pub fn missing(&self, registry: Named<'_>) -> anyhow::Result<AuthorizationError> {
        self.state().authorization_error(&*self.shell, registry, None, Reason::TokenMissing)
    }

    /// Gives the providers a token to keep (cargo's `auth::login`): `token` the one given, else
    /// one each provider asks for itself; `args` what followed `cargo login --`.
    pub fn login(
        &self,
        registry: Named<'_>,
        token: Option<&str>,
        login_url: Option<&str>,
        args: &[&str],
    ) -> anyhow::Result<()> {
        let action = Action::Login { token, login_url };
        match self.state().action(&*self.shell, registry, &action, &[], args, false)? {
            Response::Login => Ok(()),
            other => bail!(
                "credential provider produced unexpected response for `login` request: {other:?}"
            ),
        }
    }

    /// Has the providers forget the registry's token (cargo's `auth::logout`).
    pub fn logout(&self, registry: Named<'_>) -> anyhow::Result<()> {
        let answer = self.state().action(&*self.shell, registry, &Action::Logout, &[], &[], false);
        match answer {
            Err(e) if e.is::<NotFound>() => {
                self.shell.status(
                    "Logout",
                    &format!("not currently logged in to `{}`", registry.shown()),
                );
                Ok(())
            }
            Ok(Response::Logout) => Ok(()),
            Ok(other) => bail!(
                "credential provider produced unexpected response for `logout` request: {other:?}"
            ),
            Err(e) => Err(e),
        }
    }
}

impl State {
    /// cargo's `auth_token_optional`.
    fn token_optional(
        &mut self,
        // dyn: the CLI's output, as on `Auth::shell`.
        shell: &dyn Shell,
        registry: Named<'_>,
        operation: Operation<'_>,
        headers: &[String],
        require_provider: bool,
    ) -> anyhow::Result<Option<String>> {
        let key = registry.source.canonical_url().raw_canonicalized_url().to_string();
        if let Some(cached) = self.tokens.get(&key) {
            let fresh =
                cached.expiration.is_none_or(|it| SystemTime::now() + Duration::from_secs(60) < it);
            if !fresh {
                self.tokens.remove(&key);
            } else if cached.operation_independent || matches!(operation, Operation::Read) {
                return Ok(Some(cached.token.clone()));
            }
        }
        let answer = match self.action(
            shell,
            registry,
            &Action::Get(operation),
            headers,
            &[],
            require_provider,
        ) {
            Err(e) if e.is::<NotFound>() => return Ok(None),
            answer => answer?,
        };
        let Response::Get { token, cache, operation_independent } = answer else {
            bail!("credential provider produced unexpected response for `get` request: {answer:?}")
        };
        let expiration = match cache {
            CacheControl::Expires(at) => Some(at),
            CacheControl::Session => None,
            CacheControl::Never => return Ok(Some(token)),
        };
        let cached = Cached { token: token.clone(), expiration, operation_independent };
        self.tokens.insert(key, cached);
        Ok(Some(token))
    }

    /// cargo's `credential_action`: each provider in turn, until one answers.
    fn action(
        &mut self,
        // dyn: the CLI's output, as on `Auth::shell`.
        shell: &dyn Shell,
        registry: Named<'_>,
        action: &Action<'_>,
        headers: &[String],
        extra: &[&str],
        require_provider: bool,
    ) -> anyhow::Result<Response> {
        let providers = self.providers(shell, registry, require_provider, true)?;
        let mut any_not_found = false;
        for provider in providers {
            let args: Vec<&str> =
                provider.iter().map(String::as_str).chain(extra.iter().copied()).collect();
            let program = args[0];
            if not_run_here(program) {
                bail!(
                    "credential provider `{}` is not one blaze runs yet: blaze runs \
                     `cargo:token`, `cargo:token-from-stdout` and credential processes\n\
                     name one of those for {} in `registry.global-credential-providers` or \
                     its `credential-provider`",
                    args.join(" "),
                    registry.described()
                );
            }
            if shell.verbose() {
                shell.status(
                    "Credential",
                    &format!("{} {action} {}", args.join(" "), registry.shown()),
                );
            }
            let info = protocol::Registry {
                index_url: registry.source.url().as_str(),
                name: registry.name,
                headers,
            };
            let answer = match program {
                TOKEN => self.cargo_token(shell, registry, action),
                "cargo:token-from-stdout" => {
                    process::token_from_stdout(&info, action, &args[1..], self.config.cwd())
                }
                // A built-in provider of another platform answers for no registry.
                _ if BUILT_IN.contains(&program) => Err(Failure::UrlNotSupported),
                path => {
                    process::credential_process(path, &info, action, &args[1..], self.config.cwd())
                }
            };
            match answer {
                Ok(response) => return Ok(response),
                Err(Failure::UrlNotSupported) => {}
                Err(Failure::NotFound) => any_not_found = true,
                Err(Failure::Fatal(e)) => {
                    return Err(e.context(format!(
                        "credential provider `{}` failed action `{action}`",
                        args.join(" ")
                    )));
                }
            }
        }
        match any_not_found {
            true => Err(NotFound.into()),
            false => bail!("no credential providers could handle the request"),
        }
    }

    /// cargo's `TokenCredential`: the token of the configuration, written to and removed from
    /// `credentials.toml` by `login` and `logout`.
    fn cargo_token(
        &mut self,
        // dyn: the CLI's output, as on `Auth::shell`.
        shell: &dyn Shell,
        registry: Named<'_>,
        action: &Action<'_>,
    ) -> Result<Response, Failure> {
        let previous = self.credentials(shell, registry)?.and_then(|it| it.token);
        let crates_io = registry.source.is_crates_io();
        let shown = registry.shown();
        match action {
            Action::Get(_) => {
                let token = previous.ok_or(Failure::NotFound)?.val;
                Ok(Response::Get {
                    token,
                    cache: CacheControl::Session,
                    operation_independent: true,
                })
            }
            Action::Login { token, login_url } => {
                let token = match token {
                    Some(token) => (*token).to_owned(),
                    None => {
                        match (login_url, registry.name) {
                            (Some(url), _) => {
                                eprintln!("please paste the token found on {url} below")
                            }
                            (None, Some(name)) => {
                                eprintln!("please paste the token for {name} below")
                            }
                            (None, None) => eprintln!(
                                "please paste the token for {} below",
                                registry.source.url()
                            ),
                        }
                        read_line().map_err(anyhow::Error::from)?
                    }
                };
                let token = token.replace("cargo login", "").trim().to_owned();
                check_token(&token).map_err(anyhow::Error::from)?;
                let alternative = match crates_io {
                    true => None,
                    false => Some(registry.name.ok_or_else(|| {
                        anyhow::anyhow!("can't save credentials for anonymous registry")
                    })?),
                };
                self.config.save_credentials(Some(&token), alternative)?;
                shell.status("Login", &format!("token for `{shown}` saved"));
                Ok(Response::Login)
            }
            Action::Logout => {
                if previous.is_none() {
                    return Err(Failure::NotFound);
                }
                let alternative = (!crates_io).then_some(registry.name).flatten();
                self.config.save_credentials(None, alternative)?;
                shell.status(
                    "Logout",
                    &format!("token for `{shown}` has been removed from local storage"),
                );
                let location = match crates_io {
                    true => "<https://crates.io/me>".to_owned(),
                    false => format!("the `{shown}` website"),
                };
                eprintln!(
                    "note: This does not revoke the token on the registry server.\n    If you need \
                     to revoke the token, visit {location} and follow the instructions there."
                );
                Ok(Response::Logout)
            }
        }
    }

    /// cargo's `AuthorizationError::new`.
    fn authorization_error(
        &mut self,
        // dyn: the CLI's output, as on `Auth::shell`.
        shell: &dyn Shell,
        registry: Named<'_>,
        login_url: Option<&str>,
        reason: Reason,
    ) -> anyhow::Result<AuthorizationError> {
        let cargo_token =
            self.providers(shell, registry, false, false)?.iter().any(|it| it[0] == TOKEN);
        let token_lacks_scheme = self
            .tokens
            .get(registry.source.canonical_url().raw_canonicalized_url().as_str())
            .map(|it| !it.token.contains(' '));
        Ok(AuthorizationError {
            crates_io: registry.source.is_crates_io(),
            name: registry.name.map(str::to_owned),
            url: registry.source.url().to_string(),
            default_registry: self.config.default_registry()?.is_some(),
            login_url: login_url.map(str::to_owned),
            reason,
            cargo_token,
            token_lacks_scheme,
        })
    }

    /// cargo's `registry_credential_config_raw`, read once per registry.
    fn credentials(
        &mut self,
        // dyn: the CLI's output, as on `Auth::shell`.
        shell: &dyn Shell,
        registry: Named<'_>,
    ) -> anyhow::Result<Option<Credentials>> {
        let key = registry.source.canonical_url().raw_canonicalized_url().to_string();
        if let Some(found) = self.registries.get(&key) {
            return Ok(found.clone());
        }
        let found = self.credentials_uncached(shell, registry)?;
        self.registries.insert(key, found.clone());
        Ok(found)
    }

    /// The table of the registry's name: `[registry]` for crates.io, else the name found by its
    /// index URL among `CARGO_REGISTRIES_<NAME>_INDEX` and then `[registries]` (cargo's
    /// `registry_config` and `registry_credentials`).
    fn credentials_uncached(
        &self,
        // dyn: the CLI's output, as on `Auth::shell`.
        shell: &dyn Shell,
        registry: Named<'_>,
    ) -> anyhow::Result<Option<Credentials>> {
        let config = &self.config;
        if !registry.source.is_remote_registry() {
            bail!(
                "{} does not support API commands.\nCheck for a source-replacement in .cargo/config.",
                registry.described()
            );
        }
        let table = if registry.source.is_crates_io() {
            config.check_registry_index_not_set()?;
            Some("registry".to_owned())
        } else {
            let name = registry_name(config, registry.source)?;
            if let Some(name) = &name
                && Some(name.as_str()) != registry.name
            {
                shell.note(&format!(
                    "name of alternative registry `{}` set to `{name}`",
                    registry.source.url()
                ));
            }
            name.map(|it| format!("registries.{it}"))
        };
        let provider = match &table {
            Some(table) => {
                config.get::<Option<PathAndArgs>>(&format!("{table}.credential-provider"))?
            }
            None => None,
        };
        config.load_credentials()?;
        for warning in config.take_warnings() {
            shell.warn(&warning);
        }
        let Some(table) = table else { return Ok(None) };
        Ok(Some(Credentials { token: config.credentials_token(&table)?, provider }))
    }

    /// cargo's `credential_provider`: the providers to ask in turn, each a program and its
    /// arguments, with cargo's warnings of a token none of them reads.
    fn providers(
        &mut self,
        // dyn: the CLI's output, as on `Auth::shell`.
        shell: &dyn Shell,
        registry: Named<'_>,
        require_provider: bool,
        show_warnings: bool,
    ) -> anyhow::Result<Vec<Vec<String>>> {
        let warn = |text: String| {
            if show_warnings {
                shell.warn(&text);
            }
        };
        let found = self.credentials(shell, registry)?;
        let config = &self.config;
        let global =
            config.get::<Option<Vec<Value<String>>>>("registry.global-credential-providers")?;
        let (providers, global_defined) = match global.filter(|it| !it.is_empty()) {
            Some(list) => (
                list.iter()
                    .rev()
                    .map(|it| {
                        resolve_alias(
                            config,
                            shell,
                            PathAndArgs::from_whitespace_separated_string(it),
                        )
                    })
                    .collect(),
                true,
            ),
            None => (vec![vec![TOKEN.to_owned()]], false),
        };
        let described = registry.described();
        if let Some(found) = &found
            && let Some(provider) = &found.provider
        {
            let provider = resolve_alias(config, shell, provider.clone());
            if let Some(token) = &found.token
                && provider[0] != TOKEN
            {
                warn(format!(
                    "{described} has a token configured in {} that will be ignored because this \
                     registry is configured to use credential-provider `{}`",
                    token.definition, provider[0]
                ));
            }
            return Ok(vec![provider]);
        }
        if let Some(token) = found.as_ref().and_then(|it| it.token.as_ref())
            && !providers.iter().any(|it| it[0] == TOKEN)
        {
            warn(format!(
                "{described} has a token configured in {} that will be ignored because the \
                 `cargo:token` credential provider is not listed in \
                 `registry.global-credential-providers`",
                token.definition
            ));
        }
        if !global_defined && require_provider {
            bail!(
                "authenticated registries require a credential-provider to be configured\n\
                 see https://doc.rust-lang.org/nightly/cargo/reference/registry-authentication.html for details"
            );
        }
        Ok(providers)
    }
}

/// The `[registries]` name of a registry by its index URL: the `CARGO_REGISTRIES_<NAME>_INDEX`
/// variables first, else the configuration's tables; an error when two names have it.
fn registry_name(config: &Config<Disk>, source: &SourceId) -> anyhow::Result<Option<String>> {
    let index = source.canonical_url();
    let mut names: Vec<String> = config
        .env_vars()
        .filter_map(|(k, v)| {
            Some((k.strip_prefix("CARGO_REGISTRIES_")?.strip_suffix("_INDEX")?, v))
        })
        .filter(|(_, v)| registry_source(v).is_ok_and(|it| it.canonical_url() == index))
        .map(|(k, _)| k.to_lowercase())
        .collect();
    if names.is_empty() {
        #[derive(serde_derive::Deserialize)]
        struct Index {
            index: Option<String>,
        }
        let registries = config
            .get::<Option<std::collections::BTreeMap<String, Index>>>("registries")
            .ok()
            .flatten();
        names.extend(registries.into_iter().flatten().filter_map(|(name, it)| {
            let it = registry_source(it.index.as_deref()?).ok()?;
            (it.canonical_url() == index).then_some(name)
        }));
    }
    names.sort();
    match names.len() {
        0 => Ok(None),
        1 => Ok(names.pop()),
        _ => bail!(
            "multiple registries are configured with the same index url '{}': {}",
            source.url(),
            names.join(", ")
        ),
    }
}

/// cargo's `resolve_credential_alias`: a provider of no arguments named in
/// `[credential-alias]`, unless that would shadow a built-in one; the program resolved.
// dyn: the CLI's output, as on `Auth::shell`.
fn resolve_alias(config: &Config<Disk>, shell: &dyn Shell, mut provider: PathAndArgs) -> Vec<String> {
    if provider.args.is_empty() {
        let name = provider.path.raw_value().to_owned();
        let key = format!("credential-alias.{name}");
        if let Ok(Some(alias)) = config.get::<Option<PathAndArgs>>(&key) {
            if BUILT_IN.contains(&name.as_str()) {
                shell.warn(&format!(
                    "credential-alias `{name}` (defined in `{}`) will be ignored because it \
                     would shadow a built-in credential-provider",
                    alias.path.value().definition
                ));
            } else {
                provider = alias;
            }
        }
    }
    let program = provider.path.resolve_program(config.cwd()).display().to_string();
    std::iter::once(program).chain(provider.args).collect()
}

/// The `login_url` of a `www-authenticate` header's challenge of the `Cargo` scheme (RFC 7235
/// challenges, as the `http-auth` crate cargo uses reads them): the last one when there are
/// several, and an error when it is no URL. Reading stops at what is no challenge.
pub fn login_url(header: &str) -> anyhow::Result<Option<String>> {
    fn token(text: &str) -> (&str, &str) {
        let end = text
            .find(|c: char| !(c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)))
            .unwrap_or(text.len());
        text.split_at(end)
    }
    fn quoted(text: &str) -> Option<(String, &str)> {
        let mut out = String::new();
        let mut chars = text.char_indices();
        while let Some((at, c)) = chars.next() {
            match c {
                '"' => return Some((out, &text[at + 1..])),
                '\\' => out.push(chars.next()?.1),
                c => out.push(c),
            }
        }
        None
    }
    let blank = [' ', '\t'];
    let mut found = None;
    let mut scheme: Option<&str> = None;
    let mut rest = header;
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        let (name, after) = token(rest);
        if name.is_empty() {
            break;
        }
        let after_blank = after.trim_start_matches(blank);
        let Some(value) = after_blank.strip_prefix('=').filter(|_| scheme.is_some()) else {
            scheme = Some(name);
            rest = after;
            continue;
        };
        let value = value.trim_start_matches(blank);
        let (value, after) = match value.strip_prefix('"') {
            Some(inside) => match quoted(inside) {
                Some(it) => it,
                None => break,
            },
            None => {
                let (value, after) = token(value);
                (value.to_owned(), after)
            }
        };
        if scheme.is_some_and(|it| it.eq_ignore_ascii_case("Cargo"))
            && name.eq_ignore_ascii_case("login_url")
        {
            let url = url::Url::parse(&value)
                .map_err(|e| anyhow::anyhow!("invalid url `{value}`: {e}"))?;
            found = Some(url.to_string());
        }
        rest = after;
    }
    Ok(found)
}

fn read_line() -> std::io::Result<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}
