//! A host's DNS HTTPS record (RFC 9460), asked with hickory over the system's resolvers: whether
//! it lists `h3`, on which port, and for how long that holds. Only this record: a host's
//! addresses come from the system's getaddrinfo (hyper's connector and [`super::quic`] alike),
//! because hickory reads only the global resolvers of macOS, not the scoped ones a VPN's split
//! DNS adds, nor `/etc/resolver/*` (docs/networking.md, "DNS").
//!
//! A lookup that fails (a timeout, SERVFAIL, no resolver configured) is no advert, kept for
//! [`super::adverts::DNS_MIN_TTL_SECONDS`]: the host is asked over TCP as it would be with no
//! record. That is the absence of an advert, not a fallback: HTTP/3 was never chosen.

use std::time::Duration;

use hickory_resolver::{
    TokioResolver,
    net::{DnsError, NetError, NoRecords},
    proto::rr::{
        RData, RecordType,
        rdata::{
            HTTPS,
            svcb::{Alpn, SvcParamKey, SvcParamValue},
        },
    },
};
use tracing::Instrument as _;

use super::adverts::{DNS_MIN_TTL_SECONDS, DnsAnswer};

/// The most one HTTPS lookup may take, its tries included: a request waits on it before its
/// first byte, once per host per TTL.
const LOOKUP_MOST: Duration = Duration::from_secs(2);

/// The resolver, made on the client's runtime at its first lookup.
pub(super) struct Dns {
    resolver: tokio::sync::OnceCell<Result<TokioResolver, String>>,
}

impl Dns {
    pub(super) fn new() -> Dns {
        Dns { resolver: tokio::sync::OnceCell::new() }
    }

    /// What `host`'s HTTPS record says of HTTP/3. Never an error: a failure is no advert, for
    /// the least TTL, and its reason is in the span.
    pub(super) async fn https_record(&self, host: &str) -> DnsAnswer {
        let span = tracing::info_span!(target: "blaze", "dns https record", host, h3 = tracing::field::Empty, outcome = tracing::field::Empty);
        let answer = self.ask(host).instrument(span.clone()).await;
        match &answer {
            Ok(it) => {
                span.record("h3", it.h3.map(i64::from).unwrap_or(-1));
                span.record("outcome", "answered");
            }
            Err(why) => {
                span.record("outcome", why.as_str());
            }
        }
        answer.unwrap_or(DnsAnswer { h3: None, ttl_seconds: DNS_MIN_TTL_SECONDS })
    }

    async fn ask(&self, host: &str) -> Result<DnsAnswer, String> {
        let resolver = self
            .resolver
            .get_or_init(|| async {
                let mut builder = TokioResolver::builder_tokio().map_err(|e| format!("reading the system's DNS configuration: {e}"))?;
                let options = builder.options_mut();
                options.timeout = LOOKUP_MOST;
                options.attempts = 1;
                // blaze keeps the answers itself (super::adverts), for their TTL.
                options.cache_size = 0;
                builder.build().map_err(|e| format!("making a DNS resolver: {e}"))
            })
            .await
            .as_ref()
            .map_err(Clone::clone)?;
        // A fully qualified name: no search domain is tried for a host of a URL.
        let name = format!("{}.", host.trim_end_matches('.'));
        let lookup = match tokio::time::timeout(LOOKUP_MOST, resolver.lookup(name.as_str(), RecordType::HTTPS)).await {
            Err(_) => return Err(format!("no answer in {} ms", LOOKUP_MOST.as_millis())),
            Ok(Err(NetError::Dns(DnsError::NoRecordsFound(NoRecords { negative_ttl, .. })))) => {
                return Ok(DnsAnswer { h3: None, ttl_seconds: negative_ttl.map_or(DNS_MIN_TTL_SECONDS, u64::from) });
            }
            Ok(Err(e)) => return Err(format!("{e}")),
            Ok(Ok(lookup)) => lookup,
        };
        let mut ttl: Option<u32> = None;
        let mut best: Option<(u16, u16)> = None; // (priority, port)
        let mut alias = false;
        for record in lookup.answers() {
            let RData::HTTPS(HTTPS(svcb)) = &record.data else { continue };
            ttl = Some(ttl.map_or(record.ttl, |it| it.min(record.ttl)));
            // AliasMode (priority 0) points at another name; blaze does not follow it (an
            // RRSet with one must have its ServiceMode records ignored, RFC 9460 2.4.2).
            if svcb.svc_priority == 0 {
                alias = true;
                continue;
            }
            // Another target is another host's service; "." is this one.
            if !svcb.target_name.is_root() {
                continue;
            }
            let mut h3 = false;
            let mut port = 443;
            for (key, value) in &svcb.svc_params {
                match (key, value) {
                    (SvcParamKey::Alpn, SvcParamValue::Alpn(Alpn(alpns))) => h3 = alpns.iter().any(|it| it == "h3"),
                    (SvcParamKey::Port, SvcParamValue::Port(it)) => port = *it,
                    _ => {}
                }
            }
            if h3 && best.is_none_or(|(priority, _)| svcb.svc_priority < priority) {
                best = Some((svcb.svc_priority, port));
            }
        }
        let ttl_seconds = ttl.map_or_else(
            || lookup.valid_until().saturating_duration_since(std::time::Instant::now()).as_secs(),
            u64::from,
        );
        Ok(DnsAnswer { h3: if alias { None } else { best.map(|(_, port)| port) }, ttl_seconds })
    }
}
