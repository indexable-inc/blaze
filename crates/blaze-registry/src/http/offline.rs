//! [`HyperHttp`] without the `net` feature: the seed profile (todo/bootstrap/plan.md, "The seed
//! has no C"), which builds offline from vendored sources and has no TLS. It is made as the
//! networked one is, so a build that fetches nothing runs, and every request it is asked is
//! [`NoNet`], naming the URL: never an answer from anywhere else.

use std::{fmt, sync::Arc, time::Duration};

use http::Method;

use super::{Bounds, Http, Response};

/// A request this blaze can't send: it is built without blaze-registry's `net` feature.
#[derive(Clone, Debug)]
pub struct NoNet {
    pub url: String,
}

impl fmt::Display for NoNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: this blaze is built without blaze-registry's `net` feature (the seed profile, which \
             builds offline from vendored sources), so it has no HTTP client to ask it with",
            self.url
        )
    }
}

impl std::error::Error for NoNet {}

/// The HTTP client of a blaze built without `net`: every request is [`NoNet`].
pub struct HyperHttp {
    _offline: (),
}

impl HyperHttp {
    pub fn new(_timeout: Duration) -> anyhow::Result<HyperHttp> {
        Ok(HyperHttp { _offline: () })
    }

    pub fn shared() -> anyhow::Result<Arc<HyperHttp>> {
        Ok(Arc::new(HyperHttp { _offline: () }))
    }

    /// [`NoNet`] for `url`.
    pub fn send(
        &self,
        _method: Method,
        url: &str,
        _headers: &[(&str, &str)],
        _body: Vec<u8>,
    ) -> anyhow::Result<Response> {
        Err(NoNet { url: url.to_owned() }.into())
    }

    /// [`NoNet`] for `url`.
    pub fn get_bounded(
        &self,
        url: &str,
        _headers: &[(&str, &str)],
        _bounds: Bounds,
    ) -> anyhow::Result<Response> {
        Err(NoNet { url: url.to_owned() }.into())
    }
}

impl Http for HyperHttp {
    fn get(&self, url: &str, _headers: &[(&str, &str)]) -> anyhow::Result<Response> {
        Err(NoNet { url: url.to_owned() }.into())
    }
}
