//! A GitHub webhook delivery: its signature checked against the App's secret, then its payload
//! read into an [`Event`]. Nothing in a payload is trusted before [`verify`] passes.
//!
//! The listener reads each delivery in place ([`receive_ref`]): a [`DeliveryRef`] lends its
//! strings from the request's bytes, so a ping, an ignored action or a redelivery allocates
//! nothing, and an event CI runs costs only its own strings, made when it is handed on
//! ([`EventRef::into_owned`]).

use std::{borrow::Cow, fmt};

use blaze_api::http::{HttpRequest, HttpRequestRef};
use ring::hmac;
use serde::Deserialize;

use crate::Event;

/// GitHub's cap on a delivery's payload.
pub const MOST_PAYLOAD: usize = 25 << 20;

/// A delivery whose signature checked. `event` is `None` for one CI ignores (a ping, a closed
/// PR, a deleted branch).
#[derive(PartialEq, Eq, Debug)]
pub struct Delivery {
    pub id: String,
    /// "owner/name"; empty for a delivery about no repository (an App's ping).
    pub repo: String,
    pub event: Option<Event>,
}

/// A [`Delivery`] lent from the request it came in: each string is the body's or a header's
/// own bytes, unless the payload escaped it.
#[derive(PartialEq, Eq, Debug)]
pub struct DeliveryRef<'a> {
    pub id: &'a str,
    pub repo: Cow<'a, str>,
    pub event: Option<EventRef<'a>>,
}

/// An [`Event`] lent from its payload.
#[derive(PartialEq, Eq, Debug)]
pub enum EventRef<'a> {
    Push { branch: Cow<'a, str>, commit: Cow<'a, str> },
    PullRequest { number: u64, head: Cow<'a, str>, base: Cow<'a, str>, from_fork: bool },
    Tag { name: Cow<'a, str>, commit: Cow<'a, str> },
}

impl DeliveryRef<'_> {
    /// The delivery as [`receive`] gives it, every string its own.
    pub fn into_owned(self) -> Delivery {
        // alloc: the owned delivery, for a caller keeping it past the request; the listener doesn't
        let id = self.id.to_owned();
        Delivery { id, repo: self.repo.into_owned(), event: self.event.map(EventRef::into_owned) }
    }
}

impl EventRef<'_> {
    /// The event as the service takes it: two strings of its own, the one allocation of a
    /// delivery CI runs, since it is handed to the service's thread.
    pub fn into_owned(self) -> Event {
        match self {
            EventRef::Push { branch, commit } => {
                Event::Push { branch: branch.into_owned(), commit: commit.into_owned() }
            }
            EventRef::PullRequest { number, head, base, from_fork } => {
                Event::PullRequest { number, head: head.into_owned(), base: base.into_owned(), from_fork }
            }
            EventRef::Tag { name, commit } => Event::Tag { name: name.into_owned(), commit: commit.into_owned() },
        }
    }
}

/// Why a delivery was refused, with the HTTP status to answer.
#[derive(PartialEq, Eq, Debug)]
pub struct WebhookError {
    pub status: u16,
    pub what: String,
}

impl WebhookError {
    /// Only refusals make one, so its words are never a delivery's cost.
    fn new(status: u16, what: impl Into<String>) -> WebhookError {
        WebhookError { status, what: what.into() }
    }
}

impl fmt::Display for WebhookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.what, self.status)
    }
}

impl std::error::Error for WebhookError {}

/// `X-Hub-Signature-256` ("sha256=<hex>") checked against HMAC-SHA256 of `body` under `secret`,
/// in constant time (ring::hmac::verify).
pub fn verify(secret: &[u8], body: &[u8], signature: Option<&str>) -> Result<(), WebhookError> {
    Verifier::new(secret).verify(body, signature)
}

/// The webhook secret as an HMAC key, made once: its inner and outer pads are hashed at setup,
/// not on every delivery.
pub struct Verifier {
    /// `None` for an empty secret, under which nothing checks.
    key: Option<hmac::Key>,
}

impl Verifier {
    pub fn new(secret: &[u8]) -> Verifier {
        Verifier { key: (!secret.is_empty()).then(|| hmac::Key::new(hmac::HMAC_SHA256, secret)) }
    }

    /// [`verify`] under this key.
    pub fn verify(&self, body: &[u8], signature: Option<&str>) -> Result<(), WebhookError> {
        let Some(key) = &self.key else {
            return Err(WebhookError::new(500, "the webhook secret is empty, so no signature can check"));
        };
        let signature =
            signature.ok_or_else(|| WebhookError::new(400, "the delivery has no X-Hub-Signature-256"))?;
        let tag = signature
            .strip_prefix("sha256=")
            .and_then(blaze_format::hex::decode::<32>)
            .ok_or_else(|| WebhookError::new(401, "X-Hub-Signature-256 is not sha256=<64 hex digits>"))?;
        hmac::verify(key, body, &tag).map_err(|_| {
            WebhookError::new(401, "X-Hub-Signature-256 does not match the body under the webhook secret")
        })
    }
}

/// The payload of event kind `kind` (`X-GitHub-Event`), delivery `id` (`X-GitHub-Delivery`).
///
/// One pass over the body: sonic-rs reads only the fields below (SIMD, validating what it
/// skips) without building the rest, and borrows each string from the body unless it has
/// escapes. The only copies are the [`Delivery`]'s own strings.
pub fn parse(kind: &str, id: &str, body: &[u8]) -> Result<Delivery, WebhookError> {
    parse_ref(kind, id, body).map(DeliveryRef::into_owned)
}

/// [`parse`], lent from `id` and `body`: nothing is allocated unless the payload escaped a
/// string CI reads.
pub fn parse_ref<'a>(kind: &str, id: &'a str, body: &'a [u8]) -> Result<DeliveryRef<'a>, WebhookError> {
    parse_by::<Sonic>(kind, id, body)
}

/// A JSON decoder for [`parse`].
pub(crate) trait Json {
    type Error: fmt::Display;
    fn from_slice<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, Self::Error>;
}

pub(crate) struct Sonic;

impl Json for Sonic {
    type Error = sonic_rs::Error;
    fn from_slice<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, sonic_rs::Error> {
        sonic_rs::from_slice(body)
    }
}

pub(crate) fn parse_by<'a, J: Json>(
    kind: &str,
    id: &'a str,
    body: &'a [u8],
) -> Result<DeliveryRef<'a>, WebhookError> {
    if body.len() > MOST_PAYLOAD {
        return Err(WebhookError::new(
            413,
            // alloc: error path
            format!("the payload has {} bytes, over GitHub's cap of {MOST_PAYLOAD}", body.len()),
        ));
    }
    if id.is_empty() {
        return Err(WebhookError::new(400, "the delivery's X-GitHub-Delivery is empty"));
    }
    match kind {
        "push" => {
            let push: PushPayload<'_> = payload::<J, _>(kind, body)?;
            let repo = push.repository.full_name;
            if push.deleted {
                return Ok(DeliveryRef { id, repo, event: None });
            }
            let commit = sha(kind, "after", push.after)?;
            let event = if let Some(branch) = after(&push.reference, "refs/heads/") {
                Some(EventRef::Push { branch: name(kind, "ref", branch)?, commit })
            } else if let Some(tag) = after(&push.reference, "refs/tags/") {
                Some(EventRef::Tag { name: name(kind, "ref", tag)?, commit })
            } else {
                None
            };
            Ok(DeliveryRef { id, repo, event })
        }
        "pull_request" => {
            let pr: PullRequestPayload<'_> = payload::<J, _>(kind, body)?;
            if !matches!(&*pr.action, "opened" | "synchronize" | "reopened" | "ready_for_review") {
                let repo = pr.repository.map(|it| it.full_name).unwrap_or_default();
                return Ok(DeliveryRef { id, repo, event: None });
            }
            // alloc: error path
            let missing = |field: &str| WebhookError::new(400, format!("the {kind} payload has no {field}"));
            let repository = pr.repository.ok_or_else(|| missing("repository"))?;
            let number = pr.number.ok_or_else(|| missing("number"))?;
            let pull = pr.pull_request.ok_or_else(|| missing("pull_request"))?;
            let base_repo = pull.base.repo.ok_or_else(|| missing("pull_request.base.repo"))?;
            let from_fork = pull.head.repo.is_none_or(|head| head.full_name != base_repo.full_name);
            let event = EventRef::PullRequest {
                number,
                head: sha(kind, "pull_request.head.sha", pull.head.sha)?,
                base: sha(kind, "pull_request.base.sha", pull.base.sha)?,
                from_fork,
            };
            Ok(DeliveryRef { id, repo: repository.full_name, event: Some(event) })
        }
        _ => {
            let other: OtherPayload<'_> = payload::<J, _>(kind, body)?;
            let repo = other.repository.map(|it| it.full_name).unwrap_or_default();
            Ok(DeliveryRef { id, repo, event: None })
        }
    }
}

/// [`verify`] then [`parse`], from the request's headers. Only `POST`.
pub fn receive(secret: &[u8], request: &HttpRequest) -> Result<Delivery, WebhookError> {
    let verifier = Verifier::new(secret);
    receive_parts(&verifier, &request.method, |name| request.header(name), &request.body)
        .map(DeliveryRef::into_owned)
}

/// [`receive`] of a request read in place ([`RequestParser::next_ref`]) under a key made once:
/// its head and body are neither copied nor allocated, and the delivery is lent from them.
///
/// [`RequestParser::next_ref`]: blaze_api::http::RequestParser::next_ref
pub fn receive_ref<'a>(
    verifier: &Verifier,
    request: &HttpRequestRef<'a>,
) -> Result<DeliveryRef<'a>, WebhookError> {
    receive_parts(verifier, request.method, |name| request.header(name), request.body)
}

fn receive_parts<'a>(
    verifier: &Verifier,
    method: &str,
    header: impl Fn(&str) -> Option<&'a str>,
    body: &'a [u8],
) -> Result<DeliveryRef<'a>, WebhookError> {
    if method != "POST" {
        // alloc: error path
        return Err(WebhookError::new(405, format!("a webhook delivery is a POST, this was a {method}")));
    }
    verifier.verify(body, header("X-Hub-Signature-256"))?;
    if let Some(kind) = header("Content-Type") {
        let essence = kind.split(';').next().unwrap_or_default().trim();
        if !essence.eq_ignore_ascii_case("application/json") {
            return Err(WebhookError::new(
                415,
                // alloc: error path
                format!("the webhook sends {kind}; set its content type to application/json"),
            ));
        }
    }
    let kind = header("X-GitHub-Event")
        .ok_or_else(|| WebhookError::new(400, "the delivery has no X-GitHub-Event"))?;
    let id = header("X-GitHub-Delivery")
        .ok_or_else(|| WebhookError::new(400, "the delivery has no X-GitHub-Delivery"))?;
    parse_by::<Sonic>(kind, id, body)
}

fn payload<'a, J: Json, T: Deserialize<'a>>(kind: &str, body: &'a [u8]) -> Result<T, WebhookError> {
    J::from_slice(body)
        // alloc: error path
        .map_err(|error| WebhookError::new(400, format!("the {kind} payload is not one: {error}")))
}

/// `text` past `prefix`, still lent from the body when it was.
fn after<'a>(text: &Cow<'a, str>, prefix: &str) -> Option<Cow<'a, str>> {
    match text {
        Cow::Borrowed(text) => text.strip_prefix(prefix).map(Cow::Borrowed),
        // alloc: a ref the payload escaped, which GitHub's never are
        Cow::Owned(text) => text.strip_prefix(prefix).map(|rest| Cow::Owned(rest.to_owned())),
    }
}

/// A commit's full hex name, SHA-1 or SHA-256.
fn sha<'a>(kind: &str, field: &str, sha: Cow<'a, str>) -> Result<Cow<'a, str>, WebhookError> {
    let hex = sha.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'));
    if hex && matches!(sha.len(), 40 | 64) {
        Ok(sha)
    } else {
        // alloc: error path
        Err(WebhookError::new(400, format!("the {kind} payload's {field} {sha:?} is not a commit")))
    }
}

fn name<'a>(kind: &str, field: &str, name: Cow<'a, str>) -> Result<Cow<'a, str>, WebhookError> {
    if name.is_empty() || name.bytes().any(|it| it.is_ascii_control()) {
        // alloc: error path
        Err(WebhookError::new(400, format!("the {kind} payload's {field} {name:?} names nothing")))
    } else {
        Ok(name)
    }
}

#[derive(Deserialize)]
struct Repository<'a> {
    #[serde(borrow)]
    full_name: Cow<'a, str>,
}

#[derive(Deserialize)]
struct PushPayload<'a> {
    #[serde(rename = "ref", borrow)]
    reference: Cow<'a, str>,
    #[serde(borrow)]
    after: Cow<'a, str>,
    #[serde(default)]
    deleted: bool,
    #[serde(borrow)]
    repository: Repository<'a>,
}

/// Every action's payload: the fields past `action` are required only of the actions CI runs.
#[derive(Deserialize)]
struct PullRequestPayload<'a> {
    #[serde(borrow)]
    action: Cow<'a, str>,
    number: Option<u64>,
    #[serde(borrow)]
    pull_request: Option<PullRequest<'a>>,
    #[serde(borrow)]
    repository: Option<Repository<'a>>,
}

#[derive(Deserialize)]
struct PullRequest<'a> {
    #[serde(borrow)]
    head: Side<'a>,
    #[serde(borrow)]
    base: Side<'a>,
}

#[derive(Deserialize)]
struct Side<'a> {
    #[serde(borrow)]
    sha: Cow<'a, str>,
    /// Null when the fork the head is in was deleted.
    #[serde(borrow)]
    repo: Option<Repository<'a>>,
}

#[derive(Deserialize)]
struct OtherPayload<'a> {
    #[serde(borrow)]
    repository: Option<Repository<'a>>,
}

#[cfg(test)]
mod tests;
