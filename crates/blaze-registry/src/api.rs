//! A registry's web API (<https://doc.rust-lang.org/cargo/reference/registry-web-api.html>),
//! asked as the `crates-io` crate 0.41 that cargo 1.99 uses asks it: the same requests, headers
//! and bodies, and its errors in its words. No request is tried again, as cargo tries none.

use std::{fmt, sync::Arc};

use http::{Method, StatusCode, header};
use serde::{Deserialize, Serialize};

use crate::http::{HyperHttp, Response};

/// The API at `host`, with the token to send, if there is one.
pub struct Api {
    host: String,
    token: Option<String>,
    http: Arc<HyperHttp>,
    /// The registry's `auth-required`: every request carries the token.
    auth_required: bool,
}

/// A package a search found.
#[derive(Deserialize)]
pub struct Crate {
    pub name: String,
    pub description: Option<String>,
    pub max_version: String,
}

/// An owner of a package.
#[derive(Deserialize)]
pub struct User {
    pub login: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Deserialize)]
struct Ok {
    ok: bool,
}

#[derive(Deserialize)]
struct OwnerResponse {
    ok: bool,
    msg: String,
}

#[derive(Deserialize)]
struct ApiErrorList {
    errors: Vec<ApiErrorDetail>,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    detail: String,
}

#[derive(Serialize)]
struct OwnersReq<'a> {
    users: &'a [&'a str],
}

#[derive(Deserialize)]
struct Users {
    users: Vec<User>,
}

#[derive(Deserialize)]
struct Crates {
    crates: Vec<Crate>,
    meta: Total,
}

#[derive(Deserialize)]
struct Total {
    total: u32,
}

/// What went wrong asking the API, in the words of `crates_io::Error`.
#[derive(Debug)]
pub enum Error {
    /// An answer whose body had `errors[].detail`.
    Api {
        code: StatusCode,
        errors: Vec<String>,
    },
    /// Any other answer that is not a success.
    Code {
        code: StatusCode,
        headers: Vec<String>,
        body: String,
    },
    /// An answer the request's type does not read.
    Json(serde_json::Error),
    /// A body that is not UTF-8.
    Utf8,
    /// An answer that is a success whose `ok` is false.
    NotOk,
    Token(TokenError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api { code, errors } => {
                let status =
                    if code.is_success() { String::new() } else { format!(" (status {code})") };
                write!(
                    f,
                    "the remote server responded with an error{status}: {}",
                    errors.join(", ")
                )
            }
            Error::Code { code, headers, body } => write!(
                f,
                "failed to get a 200 OK response, got {}\nheaders:\n\t{}\nbody:\n{body}",
                code.as_u16(),
                headers.join("\n\t"),
            ),
            Error::Json(e) => fmt::Display::fmt(e, f),
            Error::Utf8 => f.write_str("invalid response body from server"),
            Error::NotOk => f.write_str("the registry answered that the request failed"),
            Error::Token(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for Error {}

/// A token that cannot be sent, as `crates_io::TokenError` says it.
#[derive(Debug)]
pub enum TokenError {
    Missing,
    Empty,
    InvalidCharacters,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TokenError::Missing => "no upload token found, please run `cargo login`",
            TokenError::Empty => "please provide a non-empty token",
            TokenError::InvalidCharacters => {
                "token contains invalid characters.\nOnly printable ISO-8859-1 characters are \
                 allowed as it is sent in a HTTPS header."
            }
        })
    }
}

impl std::error::Error for TokenError {}

/// Whether `token` can go in a header: not empty, and visible US-ASCII, space or tab.
pub fn check_token(token: &str) -> Result<(), TokenError> {
    if token.is_empty() {
        return Err(TokenError::Empty);
    }
    match token.bytes().all(|b| (32..127).contains(&b) || b == b'\t') {
        true => Ok(()),
        false => Err(TokenError::InvalidCharacters),
    }
}

impl Api {
    pub fn new(
        host: String,
        token: Option<String>,
        http: Arc<HyperHttp>,
        auth_required: bool,
    ) -> Api {
        Api { host, token, http, auth_required }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Up to `limit` packages matching `query`, and how many match in all.
    pub fn search(&self, query: &str, limit: u32) -> anyhow::Result<(Vec<Crate>, u32)> {
        let query =
            percent_encoding::percent_encode(query.as_bytes(), percent_encoding::NON_ALPHANUMERIC);
        let path = format!("/crates?q={query}&per_page={limit}");
        let body = self.req(Method::GET, &path, None, false)?;
        let crates: Crates = serde_json::from_str(&body).map_err(Error::Json)?;
        Ok((crates.crates, crates.meta.total))
    }

    pub fn list_owners(&self, krate: &str) -> anyhow::Result<Vec<User>> {
        let body = self.req(Method::GET, &format!("/crates/{krate}/owners"), None, true)?;
        Ok(serde_json::from_str::<Users>(&body).map_err(Error::Json)?.users)
    }

    /// The registry's message on inviting `owners`.
    pub fn add_owners(&self, krate: &str, owners: &[&str]) -> anyhow::Result<String> {
        let request = serde_json::to_string(&OwnersReq { users: owners })?;
        let path = format!("/crates/{krate}/owners");
        let body = self.req(Method::PUT, &path, Some(request.as_bytes()), true)?;
        let answer: OwnerResponse = serde_json::from_str(&body).map_err(Error::Json)?;
        if !answer.ok {
            return Err(Error::NotOk.into());
        }
        Ok(answer.msg)
    }

    pub fn remove_owners(&self, krate: &str, owners: &[&str]) -> anyhow::Result<()> {
        let request = serde_json::to_string(&OwnersReq { users: owners })?;
        let path = format!("/crates/{krate}/owners");
        let body = self.req(Method::DELETE, &path, Some(request.as_bytes()), true)?;
        let answer: OwnerResponse = serde_json::from_str(&body).map_err(Error::Json)?;
        if !answer.ok {
            return Err(Error::NotOk.into());
        }
        Ok(())
    }

    pub fn yank(&self, krate: &str, version: &str) -> anyhow::Result<()> {
        let body =
            self.req(Method::DELETE, &format!("/crates/{krate}/{version}/yank"), None, true)?;
        ok(&body)
    }

    pub fn unyank(&self, krate: &str, version: &str) -> anyhow::Result<()> {
        let body =
            self.req(Method::PUT, &format!("/crates/{krate}/{version}/unyank"), None, true)?;
        ok(&body)
    }

    fn api_url(&self, path: &str) -> String {
        match self.host.strip_prefix("file:///") {
            Some(file) => format!("file://localhost/{file}/api/v1{path}"),
            None => format!("{}/api/v1{path}", self.host),
        }
    }

    fn token(&self) -> Result<&str, TokenError> {
        let token = self.token.as_deref().ok_or(TokenError::Missing)?;
        check_token(token)?;
        Ok(token)
    }

    fn req(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        authorized: bool,
    ) -> anyhow::Result<String> {
        let url = self.api_url(path);
        let mut headers = vec![(header::ACCEPT.as_str(), "application/json")];
        if body.is_some() {
            headers.push((header::CONTENT_TYPE.as_str(), "application/json"));
        }
        if self.auth_required || authorized {
            headers.push((header::AUTHORIZATION.as_str(), self.token().map_err(Error::Token)?));
        }
        let answer = self.http.send(method, &url, &headers, body.unwrap_or_default().to_vec())?;
        Ok(handle(answer)?)
    }
}

fn ok(body: &str) -> anyhow::Result<()> {
    match serde_json::from_str::<Ok>(body).map_err(Error::Json)?.ok {
        true => Ok(()),
        false => Err(Error::NotOk.into()),
    }
}

/// The body of a success; an answer with `errors` or of another status is an [`Error`].
fn handle(answer: Response) -> Result<String, Error> {
    let body = String::from_utf8(answer.body).map_err(|_| Error::Utf8)?;
    let errors = serde_json::from_str::<ApiErrorList>(&body)
        .ok()
        .map(|it| it.errors.into_iter().map(|it| it.detail).collect::<Vec<_>>());
    let code = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match errors {
        None if code.is_success() => Ok(body),
        Some(errors) => Err(Error::Api { code, errors }),
        None => {
            let headers =
                answer.headers.iter().map(|(name, value)| format!("{name}: {value}")).collect();
            Err(Error::Code { code, headers, body })
        }
    }
}

/// Whether the API at `host` is crates.io's.
pub fn is_crates_io(host: &str) -> bool {
    url::Url::parse(host).is_ok_and(|it| it.host_str() == Some("crates.io"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_that_go_in_a_header() {
        assert!(check_token("abc def\t1").is_ok());
        assert!(matches!(check_token(""), Err(TokenError::Empty)));
        assert!(matches!(check_token("caf\u{e9}"), Err(TokenError::InvalidCharacters)));
        assert!(matches!(check_token("a\nb"), Err(TokenError::InvalidCharacters)));
    }

    #[test]
    fn errors_as_cargo_says_them() {
        let answer = |status, body: &str| Response {
            status,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: body.as_bytes().to_vec(),
        };
        let api = handle(answer(404, r#"{"errors":[{"detail":"a"},{"detail":"b"}]}"#));
        assert_eq!(
            api.unwrap_err().to_string(),
            "the remote server responded with an error (status 404 Not Found): a, b"
        );
        let code = handle(answer(500, "down"));
        assert_eq!(
            code.unwrap_err().to_string(),
            "failed to get a 200 OK response, got 500\nheaders:\n\tcontent-type: text/plain\nbody:\ndown"
        );
        assert_eq!(handle(answer(200, "{}")).unwrap(), "{}");
    }
}
