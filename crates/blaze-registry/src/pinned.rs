//! blaze-fetch's drivers over this crate's HTTP client ([`Net`]) and git checkouts ([`Git`]):
//! what [`blaze_fetch::Fetcher`] downloads and checks out through, here because blaze-fetch is
//! under this crate and cannot name its client.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use blaze_fetch::{Digest, Failure, Policy, redact, scrub};
use blaze_source::{GitReference, SourceId};

use crate::{
    Context, NetConfig,
    http::{Http, HttpNotSuccessful, Retry, Transport},
};

/// [`blaze_fetch::Net`] over an [`Http`]: a 200 or 206 streams to the sink (HyperHttp's in
/// ranges, many at once, each piece with its offset); a server error, too many requests or no
/// answer is worth another try; a download that stalled ([`Stalled`](crate::http::Stalled)) or any other answer is
/// not.
pub struct Net {
    // dyn: the I/O driver, as on `Context::http`.
    http: Arc<dyn Http>,
}

impl Net {
    // dyn: as `Net::http`
    pub fn new(http: Arc<dyn Http>) -> Net {
        Net { http }
    }
}

impl blaze_fetch::Net for Net {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        from: u64,
        sink: &mut (dyn FnMut(u64, &[u8]) -> std::io::Result<()> + Send),
    ) -> Result<(), Failure> {
        match self.http.stream(url, headers, from, sink) {
            Ok(answer) if matches!(answer.status, 200 | 206) => Ok(()),
            // Every message is scrubbed of URLs' credentials and queries: the client's own say
            // its address redacted, and a fake's or a future client's may not.
            Ok(answer) => {
                let refused = HttpNotSuccessful::new(&redact(url), &answer);
                let said = scrub(&refused.to_string()); // alloc: error path
                Err(if refused.is_spurious() {
                    Failure::Transient(said)
                } else {
                    Failure::Permanent(said)
                })
            }
            Err(e) if e.downcast_ref::<Transport>().is_some() => {
                Err(Failure::Transient(scrub(&format!("{e:#}"))))
            } // alloc: error path
            Err(e) => Err(Failure::Permanent(scrub(&format!("{e:#}")))), // alloc: error path
        }
    }
}

/// cargo's `net.retry` as blaze-fetch's tries: `retries` more after the first, half a second
/// doubling to ten between, or the fixed wait a test sets. cargo's first-wait jitter and a
/// `Retry-After` are not kept (the handoff lists them).
pub fn policy(retry: &Retry) -> Policy {
    let fixed = retry.fixed_sleep_ms.map(Duration::from_millis);
    Policy {
        tries: retry.retries.saturating_add(1),
        first_wait: fixed.unwrap_or(Duration::from_millis(500)),
        most_wait: fixed.unwrap_or(Duration::from_secs(10)),
    }
}

/// `checksum`, an index's or a lock file's sha256 in hex, as a pin.
pub fn crate_pin(checksum: &str) -> Option<Digest> {
    let bytes: [u8; 32] = blaze_format::hex::decode(checksum)?;
    Digest::new(blaze_fetch::Algo::Sha256, &bytes)
}

/// [`blaze_fetch::Git`] over [`crate::git::fetch`]: the commit cloned and checked out under
/// the fetch's own scratch directory, as the cargo home of that fetch alone (its `git/db` and
/// `git/checkouts`), so no database, checkout or lock is shared with another fetch or with the
/// user's cargo, and all of it goes when the scratch directory does. Its `.cargo-ok` is left
/// out of the tree. What is kept is the tree, in the store; a later fetch of the same pin never
/// clones.
pub struct Git {
    net: NetConfig,
    // dyn: as `Net::http`
    http: Arc<dyn Http>,
}

impl Git {
    // dyn: as `Net::http`
    pub fn new(net: NetConfig, http: Arc<dyn Http>) -> Git {
        Git { net, http }
    }
}

impl blaze_fetch::Git for Git {
    /// Every error is [`scrub`]bed: git's and the URL parser's messages carry the repository's
    /// address, credentials and all.
    fn checkout(&self, repo: &str, rev: &str, scratch: &Path) -> Result<PathBuf, String> {
        let url =
            url::Url::parse(repo).map_err(|e| format!("`{}` is not a URL: {e}", redact(repo)))?; // alloc: error path
        // alloc: once a checkout
        let id = SourceId::for_git(&url, GitReference::Rev(rev.to_owned()))
            .map_err(|e| scrub(&format!("{e:#}")))?; // alloc: error path
        let mut context = Context::new(scratch.to_owned(), Arc::clone(&self.http)); // alloc: once a checkout
        context.net = self.net;
        // A checkout for blaze-fetch prints nothing: the daemon's stderr is serve.log, and git's
        // progress lines name the repository's address (whatever the caller's `net` says).
        context.net.retry.quiet = true;
        let checkout =
            crate::git::fetch(&context, id, Some(rev)).map_err(|e| scrub(&format!("{e:#}")))?; // alloc: error path
        if !checkout.rev.eq_ignore_ascii_case(rev) {
            return Err(format!("asked for {rev}, and the checkout is of {}", checkout.rev)); // alloc: error path
        }
        if !checkout.root.starts_with(scratch) {
            return Err(format!(
                "the checkout is at {}, outside its scratch directory {}",
                checkout.root.display(),
                scratch.display()
            )); // alloc: error path
        }
        Ok(checkout.root)
    }

    fn markers(&self) -> &[&str] {
        &[".cargo-ok"]
    }
}
