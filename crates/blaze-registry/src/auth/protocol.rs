//! The lines of cargo's credential provider protocol, version 1 (`cargo_credential`): the
//! provider's hello, the request cargo writes, and the answer it reads. No I/O here.

use std::time::{Duration, SystemTime};

use anyhow::Context;
use serde_derive::{Deserialize, Serialize};

use super::{Action, CacheControl, Failure, Response};

/// The protocol versions blaze speaks.
const VERSIONS: [u32; 1] = [1];

/// The registry a request is of, `cargo_credential::RegistryInfo`.
#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct Registry<'a> {
    pub index_url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    /// The headers of the 401 the registry answered with, `name: value` each.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub headers: &'a [String],
}

#[derive(Serialize)]
struct Request<'a> {
    v: u32,
    registry: &'a Registry<'a>,
    #[serde(flatten)]
    action: &'a Action<'a>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    args: &'a [&'a str],
}

#[derive(Deserialize)]
struct Hello {
    v: Vec<u32>,
}

/// A provider's answer, `Result<CredentialResponse, cargo_credential::Error>`.
#[derive(Deserialize)]
enum Answer {
    Ok(Got),
    Err(Refused),
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Got {
    Get {
        token: String,
        #[serde(flatten)]
        cache: Cache,
        operation_independent: bool,
    },
    Login,
    Logout,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "cache", rename_all = "kebab-case")]
enum Cache {
    Never,
    Expires {
        /// Seconds since the Unix epoch.
        expiration: i64,
    },
    Session,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Refused {
    UrlNotSupported,
    NotFound,
    OperationNotSupported,
    Other {
        message: String,
        #[serde(rename = "caused-by")]
        caused_by: Option<Vec<String>>,
    },
    #[serde(other)]
    Unknown,
}

/// The provider's first line: an error unless it speaks a version blaze does.
pub(super) fn hello(line: &str) -> anyhow::Result<()> {
    let hello: Hello = serde_json::from_str(line).context("failed to deserialize hello")?;
    if !hello.v.iter().any(|it| VERSIONS.contains(it)) {
        anyhow::bail!(
            "credential provider supports protocol versions {:?}, while Cargo supports {:?}",
            hello.v,
            VERSIONS
        );
    }
    Ok(())
}

/// The request line, without its newline.
pub(super) fn request(
    registry: &Registry<'_>,
    action: &Action<'_>,
    args: &[&str],
) -> serde_json::Result<String> {
    serde_json::to_string(&Request { v: VERSIONS[0], registry, action, args })
}

/// What the answer line says: `Ok` with what the provider did, `Err` with why it did not.
pub(super) fn answer(line: &str) -> serde_json::Result<Result<Response, Failure>> {
    Ok(match serde_json::from_str::<Answer>(line)? {
        Answer::Ok(Got::Get { token, cache, operation_independent }) => {
            let cache = match cache {
                Cache::Session => CacheControl::Session,
                Cache::Expires { expiration } => CacheControl::Expires(at(expiration)),
                Cache::Never | Cache::Unknown => CacheControl::Never,
            };
            Ok(Response::Get { token, cache, operation_independent })
        }
        Answer::Ok(Got::Login) => Ok(Response::Login),
        Answer::Ok(Got::Logout) => Ok(Response::Logout),
        Answer::Ok(Got::Unknown) => Ok(Response::Unknown),
        Answer::Err(Refused::UrlNotSupported) => Err(Failure::UrlNotSupported),
        Answer::Err(Refused::NotFound) => Err(Failure::NotFound),
        Answer::Err(Refused::OperationNotSupported) => Err(Failure::operation_not_supported()),
        Answer::Err(Refused::Other { message, caused_by }) => {
            let mut chain = caused_by.unwrap_or_default();
            chain.insert(0, message);
            let mut inward = chain.into_iter().rev();
            let innermost = anyhow::Error::msg(inward.next().expect("the message is in the chain"));
            Err(Failure::Fatal(inward.fold(innermost, anyhow::Error::context)))
        }
        Answer::Err(Refused::Unknown) => {
            Err(Failure::Fatal(anyhow::anyhow!("unknown error kind; try updating Cargo?")))
        }
    })
}

fn at(seconds: i64) -> SystemTime {
    match u64::try_from(seconds) {
        Ok(after) => SystemTime::UNIX_EPOCH + Duration::from_secs(after),
        Err(_) => SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs()),
    }
}
