//! The cache's settings, read once at the entry point.
//!
//! - `<root>/remote.json`: what the cache is. No file, or `"enabled": false`, means blaze
//!   contacts nothing ([`Settings::contacts`] is `None`). Example shape, with no real value:
//!   `{"enabled": true, "bucket": {"endpoint": "https://<account>.r2.cloudflarestorage.com",
//!   "name": "<bucket>", "public_url": "https://<public host>", "cas": true}, "namespaces":
//!   [{"name": "public", "anchors": ["<64 hex>"]}], "root": "main", "sample_per_1024": 64}`.
//! - `<root>/remote-credentials.json`, owner-only (mode 0600, refused otherwise), for a machine
//!   that writes: `{"access_key_id": ..., "secret_access_key": ..., "signing_seed": ...,
//!   "key_list": "<path>"}`.
//! - In CI the same come from the environment: `BLAZE_CACHE_ENDPOINT`, `BLAZE_CACHE_BUCKET`,
//!   `BLAZE_CACHE_PUBLIC_URL`, `BLAZE_CACHE_PUBLIC_ANCHORS` (comma-separated hex),
//!   `BLAZE_CACHE_ACCESS_KEY_ID`, `BLAZE_CACHE_SECRET_ACCESS_KEY`, `BLAZE_CACHE_SIGNING_SEED`,
//!   `BLAZE_CACHE_KEY_LIST`. A value set both in a file and in the environment is refused,
//!   naming both.
//!
//! Secrets are held as [`Secret`], whose Debug and Display are `<redacted>`. Nothing here
//! prints a secret, and an error about one names the setting, never its value.

use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

use blaze_cas::plain::Namespace;
use serde::Deserialize;

use crate::{
    name,
    trust::{PublicKey, Signer},
};

pub const FILE: &str = "remote.json";
pub const CREDENTIALS: &str = "remote-credentials.json";

/// The environment, as the entry point reads it.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// A secret: shown as `<redacted>`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(text: String) -> Secret {
        Secret(text)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileForm {
    #[serde(default = "yes")]
    enabled: bool,
    bucket: Option<BucketForm>,
    #[serde(default)]
    namespaces: Vec<NamespaceForm>,
    root: Option<String>,
    sample_per_1024: Option<u32>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BucketForm {
    endpoint: Option<String>,
    name: Option<String>,
    public_url: Option<String>,
    cas: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamespaceForm {
    name: String,
    anchors: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CredentialsForm {
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    signing_seed: Option<String>,
    key_list: Option<PathBuf>,
}

/// The bucket.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BucketSettings {
    /// The S3 endpoint, for signed requests (writes, and reads of a private namespace).
    pub endpoint: Option<String>,
    pub name: Option<String>,
    /// Where public objects are read without a login.
    pub public_url: Option<String>,
    pub cas: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NamespaceSettings {
    pub name: String,
    pub namespace: Namespace,
    pub anchors: Vec<PublicKey>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret: Secret,
}

/// What can leave this machine, as `blaze cache status` says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Leaves {
    /// Nothing: no cache configured, or it is disabled.
    Nothing,
    /// Only requests for objects by digest and name (reads): no source, name of the
    /// workspace's, or record leaves.
    Reads,
    /// Pushes too: records of the namespaces listed go to the bucket.
    Pushes,
}

#[derive(Debug)]
pub struct Settings {
    pub enabled: bool,
    pub bucket: Option<BucketSettings>,
    pub namespaces: Vec<NamespaceSettings>,
    pub root: String,
    pub sample_per_1024: u32,
    pub credentials: Option<Credentials>,
    pub signer: Option<Signer>,
    pub key_list: Option<PathBuf>,
    /// Where each setting came from, for `status`: (setting, "file" or "environment").
    pub from: Vec<(&'static str, &'static str)>,
}

fn env_text(env: Env<'_>, name: &'static str) -> Result<Option<String>, String> {
    match env(name) {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => value.into_string().map(Some).map_err(|_| format!("{name} is not UTF-8")),
    }
}

/// One setting from a file or the environment, refused when it is in both.
fn one<T>(what: &'static str, file: Option<T>, env: Option<T>, from: &mut Vec<(&'static str, &'static str)>) -> Result<Option<T>, String> {
    match (file, env) {
        (Some(_), Some(_)) => Err(format!("{what} is set both in a settings file and in the environment: set it in one")),
        (Some(it), None) => {
            from.push((what, "file"));
            Ok(Some(it))
        }
        (None, Some(it)) => {
            from.push((what, "environment"));
            Ok(Some(it))
        }
        (None, None) => Ok(None),
    }
}

fn anchors(what: &str, texts: &[String]) -> Result<Vec<PublicKey>, String> {
    texts.iter().map(|it| PublicKey::from_hex(it).ok_or_else(|| format!("{what}: {it:?} is not 64 hex digits (an Ed25519 public key)"))).collect()
}

impl Settings {
    /// The settings of `root` and `env`.
    pub fn read(root: &Path, env: Env<'_>) -> Result<Settings, String> {
        let path = root.join(FILE);
        let form: FileForm = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileForm { enabled: true, ..FileForm::default() },
            Err(e) => return Err(format!("{}: {e}", path.display())),
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?,
        };
        let cpath = root.join(CREDENTIALS);
        let creds: CredentialsForm = match std::fs::read(&cpath) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CredentialsForm::default(),
            Err(e) => return Err(format!("{}: {e}", cpath.display())),
            Ok(bytes) => {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&cpath).map_err(|e| format!("{}: {e}", cpath.display()))?.permissions().mode();
                if mode & 0o077 != 0 {
                    return Err(format!("{}: its mode is {:o}: a file of credentials is read only when no one but its owner can read it (chmod 600)", cpath.display(), mode & 0o777));
                }
                // The error never quotes the file: it holds secrets.
                serde_json::from_slice(&bytes).map_err(|e| format!("{}: not the credentials form (line {}, column {})", cpath.display(), e.line(), e.column()))?
            }
        };
        let mut from = Vec::new();
        let bucket_form = form.bucket.unwrap_or_default();
        let endpoint = one("bucket.endpoint (BLAZE_CACHE_ENDPOINT)", bucket_form.endpoint, env_text(env, "BLAZE_CACHE_ENDPOINT")?, &mut from)?;
        let name = one("bucket.name (BLAZE_CACHE_BUCKET)", bucket_form.name, env_text(env, "BLAZE_CACHE_BUCKET")?, &mut from)?;
        let public_url = one("bucket.public_url (BLAZE_CACHE_PUBLIC_URL)", bucket_form.public_url, env_text(env, "BLAZE_CACHE_PUBLIC_URL")?, &mut from)?;
        let bucket = (endpoint.is_some() || name.is_some() || public_url.is_some()).then(|| BucketSettings { endpoint, name, public_url, cas: bucket_form.cas.unwrap_or(true) });
        for url in bucket.iter().flat_map(|it| it.endpoint.iter().chain(it.public_url.iter())) {
            if !url.starts_with("https://") || url.contains('@') || url.trim_end_matches('/').matches('/').count() != 2 {
                return Err(format!("the bucket address {url:?} is refused: it is `https://<host>` with no path and no credentials in it"));
            }
        }
        if let Some(name) = bucket.as_ref().and_then(|it| it.name.as_ref()) {
            name::part("bucket", name).map_err(|it| it.to_string())?;
        }

        let mut namespaces = Vec::new();
        for it in &form.namespaces {
            let namespace = name::namespace_named(&it.name).map_err(|e| e.to_string())?;
            namespaces.push(NamespaceSettings { name: it.name.clone(), namespace, anchors: anchors(&format!("namespace {}", it.name), &it.anchors)? });
        }
        if let Some(text) = env_text(env, "BLAZE_CACHE_PUBLIC_ANCHORS")? {
            if namespaces.iter().any(|it| it.namespace == Namespace::PUBLIC) {
                return Err("the public namespace's anchors are set both in remote.json and in BLAZE_CACHE_PUBLIC_ANCHORS: set them in one".to_owned());
            }
            let texts: Vec<String> = text.split(',').map(|it| it.trim().to_owned()).filter(|it| !it.is_empty()).collect();
            namespaces.push(NamespaceSettings { name: "public".to_owned(), namespace: Namespace::PUBLIC, anchors: anchors("BLAZE_CACHE_PUBLIC_ANCHORS", &texts)? });
            from.push(("namespaces.public.anchors (BLAZE_CACHE_PUBLIC_ANCHORS)", "environment"));
        }

        let access = one("access_key_id (BLAZE_CACHE_ACCESS_KEY_ID)", creds.access_key_id, env_text(env, "BLAZE_CACHE_ACCESS_KEY_ID")?, &mut from)?;
        let secret = one("secret_access_key (BLAZE_CACHE_SECRET_ACCESS_KEY)", creds.secret_access_key, env_text(env, "BLAZE_CACHE_SECRET_ACCESS_KEY")?, &mut from)?;
        let credentials = match (access, secret) {
            (Some(access_key_id), Some(secret)) => Some(Credentials { access_key_id, secret: Secret(secret) }),
            (None, None) => None,
            (Some(_), None) => return Err("an access key id is set without its secret access key (BLAZE_CACHE_SECRET_ACCESS_KEY)".to_owned()),
            (None, Some(_)) => return Err("a secret access key is set without its access key id (BLAZE_CACHE_ACCESS_KEY_ID)".to_owned()),
        };
        let seed = one("signing_seed (BLAZE_CACHE_SIGNING_SEED)", creds.signing_seed, env_text(env, "BLAZE_CACHE_SIGNING_SEED")?, &mut from)?;
        let signer = seed.map(|it| Signer::from_hex("the signing seed (BLAZE_CACHE_SIGNING_SEED)", &it)).transpose()?;
        let key_list = one("key_list (BLAZE_CACHE_KEY_LIST)", creds.key_list, env_text(env, "BLAZE_CACHE_KEY_LIST")?.map(PathBuf::from), &mut from)?;
        let root_name = form.root.unwrap_or_else(|| "main".to_owned());
        name::part("root", &root_name).map_err(|it| it.to_string())?;
        Ok(Settings {
            enabled: form.enabled,
            bucket,
            namespaces,
            root: root_name,
            sample_per_1024: form.sample_per_1024.unwrap_or(64).min(1024),
            credentials,
            signer,
            key_list,
            from,
        })
    }

    /// The bucket, when anything may contact it: enabled and configured. `None` means blaze
    /// contacts nothing.
    pub fn contacts(&self) -> Option<&BucketSettings> {
        self.bucket.as_ref().filter(|_| self.enabled)
    }

    /// What can leave this machine.
    pub fn leaves(&self) -> Leaves {
        match self.contacts() {
            None => Leaves::Nothing,
            Some(bucket) if bucket.endpoint.is_some() && self.credentials.is_some() && self.signer.is_some() => Leaves::Pushes,
            Some(_) => Leaves::Reads,
        }
    }

    pub fn namespace(&self, namespace: Namespace) -> Option<&NamespaceSettings> {
        self.namespaces.iter().find(|it| it.namespace == namespace)
    }
}
