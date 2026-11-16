//! The driver: one commit of a repository fetched over smart HTTP, protocol v2, through a
//! blaze-h1-net [`Client`]: the advertisement checked, the ref looked up (`ls-refs`) when a
//! name is wanted, and the commit fetched shallow (`fetch` with `deepen`) as one pack.

use core::{alloc::Allocator, fmt};

use blaze_h1_net::{Ask, Client, FetchError, Tls};
use blaze_own::{AllocFailure, Vec};
use bstd::{
    cx::Cx,
    url::{Url, UrlError},
};

use crate::{
    oid::{Kind, Oid},
    pack::{self, Pack, PackError},
    pkt::PktWhy,
    protocol,
};

/// What to fetch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want<'a> {
    /// A full ref name: `HEAD`, `refs/heads/main`, `refs/tags/v1.0`.
    Ref(&'a str),
    /// A commit by id (a lock file's pin).
    Commit(Oid),
}

/// A fetched commit and the pack holding its tree.
pub struct Fetched<A: Allocator> {
    pub commit: Oid,
    pub pack: Pack<A>,
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum GitWhy {
    /// The repository's URL with a suffix is no URL.
    Url(UrlError),
    Fetch(FetchError),
    /// The server answered other than 200.
    Status { status: u16 },
    Pkt(PktWhy),
    Pack(PackError),
    /// No ref of the wanted name.
    NoRef,
    /// The pack holds no commit of the wanted id.
    NoCommit { oid: Oid },
    Alloc(AllocFailure),
}

/// A failed fetch: the step and why.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct GitError {
    pub step: Step,
    pub why: GitWhy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Advertisement,
    LsRefs,
    Fetch,
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "git {:?}: ", self.step)?;
        match &self.why {
            GitWhy::Url(e) => write!(f, "no URL: {e:?}"),
            GitWhy::Fetch(e) => write!(f, "{e}"),
            GitWhy::Status { status } => write!(f, "the server answered {status}"),
            GitWhy::Pkt(why) => write!(f, "a protocol answer not as v2 says: {why:?}"),
            GitWhy::Pack(e) => write!(f, "{e}"),
            GitWhy::NoRef => f.write_str("no ref of that name"),
            GitWhy::NoCommit { oid } => write!(f, "the pack holds no commit {oid}"),
            GitWhy::Alloc(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for GitError {}

const PROTOCOL: (&str, &str) = ("Git-Protocol", "version=2");
const POST_HEADERS: [(&str, &str); 3] = [
    ("Content-Type", "application/x-git-upload-pack-request"),
    ("Accept", "application/x-git-upload-pack-result"),
    PROTOCOL,
];

/// `want` of `repo` (`https://github.com/owner/name`), its history cut at `depth` commits.
pub fn fetch<T: Tls, A: Allocator + Clone>(
    client: &Client<'_, T, A>,
    cx: &Cx,
    repo: &Url,
    want: Want<'_>,
    depth: u32,
    limits: pack::Limits,
) -> Result<Fetched<A>, GitError> {
    let alloc = client.alloc.clone();
    let at = |step| move |why| GitError { step, why };
    let mut body = Vec::new_in(alloc.clone());
    let refs = url(repo, "/info/refs?service=git-upload-pack", &alloc).map_err(at(Step::Advertisement))?;
    let ask = Ask { method: "GET", url: &refs, headers: &[PROTOCOL], body: &[], range: None };
    call(client, cx, &ask, &mut body).map_err(at(Step::Advertisement))?;
    protocol::check_advertisement(&body).map_err(|w| at(Step::Advertisement)(GitWhy::Pkt(w)))?;
    let service = url(repo, "/git-upload-pack", &alloc).map_err(at(Step::LsRefs))?;
    let mut request = Vec::new_in(alloc.clone());
    let commit = match want {
        Want::Commit(oid) => oid,
        Want::Ref(name) => {
            protocol::ls_refs(&[name], &mut request).map_err(|w| at(Step::LsRefs)(GitWhy::Pkt(w)))?;
            let ask = Ask { method: "POST", url: &service, headers: &POST_HEADERS, body: &request, range: None };
            body.clear();
            call(client, cx, &ask, &mut body).map_err(at(Step::LsRefs))?;
            let found = protocol::find_ref(&body, name).map_err(|w| at(Step::LsRefs)(GitWhy::Pkt(w)))?;
            found.ok_or(at(Step::LsRefs)(GitWhy::NoRef))?
        }
    };
    request.clear();
    protocol::fetch(commit, depth, &mut request).map_err(|w| at(Step::Fetch)(GitWhy::Pkt(w)))?;
    let ask = Ask { method: "POST", url: &service, headers: &POST_HEADERS, body: &request, range: None };
    body.clear();
    call(client, cx, &ask, &mut body).map_err(at(Step::Fetch))?;
    let mut bytes = Vec::new_in(alloc.clone());
    protocol::fetched_pack(&body, client.limits.most_body, &mut bytes).map_err(|w| at(Step::Fetch)(GitWhy::Pkt(w)))?;
    drop(body);
    let pack = Pack::read_in(&bytes, limits, alloc).map_err(|e| at(Step::Fetch)(GitWhy::Pack(e)))?;
    match pack.get(&commit) {
        Some(object) if object.kind == Kind::Commit => Ok(Fetched { commit, pack }),
        _ => Err(at(Step::Fetch)(GitWhy::NoCommit { oid: commit })),
    }
}

fn call<T: Tls, A: Allocator + Clone>(
    client: &Client<'_, T, A>,
    cx: &Cx,
    ask: &Ask<'_>,
    body: &mut Vec<u8, A>,
) -> Result<(), GitWhy> {
    let got = client.send(cx, ask, body).map_err(GitWhy::Fetch)?;
    match got.status {
        200 => Ok(()),
        status => Err(GitWhy::Status { status }),
    }
}

/// `repo` with `suffix` after its path (one trailing `/` dropped).
fn url<A: Allocator + Clone>(repo: &Url, suffix: &str, alloc: &A) -> Result<Url, GitWhy> {
    let base = repo.as_str();
    let base = base.strip_suffix('/').unwrap_or(base);
    let mut text = Vec::new_in(alloc.clone());
    text.try_reserve(base.len() + suffix.len()).map_err(GitWhy::Alloc)?;
    text.extend_from_slice(base.as_bytes());
    text.extend_from_slice(suffix.as_bytes());
    let text = core::str::from_utf8(&text).expect("two `str`s joined are UTF-8");
    Url::parse(text).map_err(GitWhy::Url)
}
