---
status: current
summary: blaze's one HTTP client (crates.io index and downloads, URL fetches, release downloads): TCP with HTTP/2 or HTTP/1.1, HTTP/3 over QUIC by `net.http3`, how a host's HTTP/3 is learned (DNS HTTPS records, Alt-Svc), retries, proxies, and its errors.
---
# Networking: blaze's one HTTP client

Every fetch blaze makes goes through one client, `HyperHttp` in
`crates/blaze-registry/src/http/client.rs`, behind the `Http` trait
(`crates/blaze-registry/src/http.rs`): the crates.io sparse index and `.crate` downloads, a
registry's API, blaze-fetch's URL fetches (`crates/blaze-cargo/src/serve/fetch_url.rs`), the
toolchain's library and own downloads, and the release and update checks
(`crates/blaze-engine/src/versions.rs`). The daemon holds one per process
(`HyperHttp::shared`), on a runtime of its own.

It is hyper 1 over rustls with the ring provider and the machine's root certificates. Over TCP
its ALPN offers `h2` and `http/1.1`, and the server picks; requests to one HTTP/2 host share its
connection. Over UDP it speaks HTTP/3 by quinn and h3 on the same rustls/ring provider and the
same roots, for the hosts `net.http3` and the host's advert pick (below). There is no second TLS
stack, and the seed profile (blaze-registry without its `net` feature) has none at all: every
request is `NoNet`, naming the URL.

Two other clients remain, each its own small HTTP/1 over rustls, to be moved onto this one: the
global cache's (`crates/blaze-global/src/client.rs`, sans-IO, pipelined) and the GitHub App's
(`crates/blaze-ci/src/github.rs`).

## Retries

A request that got no answer (refused, reset, timed out, cut short over TCP) or a 5xx or 429
answer is tried again as cargo's `net.retry` does (`Retry` in `crates/blaze-registry/src/http.rs`):
three more times, half a second plus jitter, then three seconds more each time to ten, or what a
429 or 503's `Retry-After` asks. A try again goes over the same protocol as the first. A failure
of HTTP/3 at the transport is not tried again (below).

## HTTP/3

`net.http3` in blaze's user configuration, `config.toml` in blaze's home (`$BLAZE_HOME`, else
`~/.blaze`), says when a request goes over HTTP/3:

```toml
[net]
http3 = "auto"              # "off" (the default), "auto" or "require"

[net.http3-hosts]
"static.crates.io" = "off"  # one host's own setting, over `http3`
```

- `"off"`: every request over TCP. The default until a benchmark (`harness/netbench.py`) shows
  HTTP/3 wins.
- `"auto"`: HTTP/3 to a host that advertises it, TCP to the rest. blaze never probes (tries
  HTTP/3 to see whether it works): it uses HTTP/3 only where the host said it serves it, which is
  negotiation, the same as ALPN picking `h2`.
- `"require"`: HTTP/3 for every request, on the advertised port or 443. A host reached through a
  proxy, or a plain `http://` address, is an error naming it.

The decision is one pure function, `choose` in `crates/blaze-registry/src/http/choice.rs`, of
the host's setting, its advert and the proxy the environment names for it; its table is in its
doc comment and its one test walks every row.

### How a host's HTTP/3 is learned

In this order, per host:

1. Its DNS HTTPS record (RFC 9460): a ServiceMode record for the host itself whose `alpn` lists
   `h3`, on its `port` (443 when it has none). Asked once per host before its first request under
   `"auto"` or `"require"` (Cloudflare publishes these, so a Cloudflare-fronted host is on HTTP/3
   from the first request). An AliasMode record, or a record for another target, is no advert.
2. An `Alt-Svc: h3=":port"; ma=N` header on any earlier answer from the host (RFC 7838), for its
   `ma` (24 hours when it gives none). `Alt-Svc: clear`, or `ma=0`, forgets it. An alternative
   on another host is not followed.
3. `BUILTIN` in `choice.rs`: hosts known to serve HTTP/3. Empty until one is measured.

What was learned, a DNS answer that lists no `h3` included, is kept per host until it expires, in
the daemon's state, `<root>/v2/net/h3-adverts.json` (`crates/blaze-registry/src/http/adverts.rs`,
JSON ordered by host). A missing file is no adverts yet; a file that can't be read is moved aside
to `h3-adverts.json.unreadable`, and blaze says so in a warning naming it and why, and starts
with none.

### DNS

A DNS answer is kept for its TTL, held to between 30 seconds (`DNS_MIN_TTL_SECONDS`) and a day
(`MAX_TTL_SECONDS`); no record is kept for its SOA's negative TTL. A lookup that fails (no answer
in 2 seconds, SERVFAIL, no resolver) is no advert, kept for the 30 seconds: HTTP/3 was never
chosen, so the request goes over TCP as for any host without a record. That is the absence of an
advert, not a fallback.

The HTTPS record is asked with hickory-resolver over the system's resolvers. A host's addresses
are not: hyper's connector and the HTTP/3 side both take them from the system's getaddrinfo,
which caches them as the system does (mDNSResponder on macOS, systemd-resolved or nscd on Linux).
hickory's macOS configuration reads only the global resolvers (`State:/Network/Global/DNS`), not
the scoped ones a VPN's split DNS adds or `/etc/resolver/*`; resolving addresses with it would
lose those hosts. getaddrinfo gives no TTL, so blaze keeps no addresses of its own.

### No fallback to TCP

Once blaze chose HTTP/3 for a host, a failure at the transport is an error, `Http3Failed`, never
a request sent again over TCP (rule no-fallback). It names the host and port, the URL, what
failed (no address; the QUIC handshake getting no answer in 10 s, which is UDP blocked; the
handshake refused; the connection or a stream reset; no answer or a stalled body within the
request timeout), why HTTP/3 was chosen (`dns`, `alt-svc`, `builtin` or `required`), and the fix:
`net.http3 = "off"` under `[net]` in `config.toml`, or the host's own `"off"` under
`[net.http3-hosts]`. It is not tried again over HTTP/3 either: the retries are for answers a
server gave. An answer over HTTP/3 (a 404, a 503) is handled as over TCP, retries included.

### Connections

One QUIC connection per host and port in the process, made at its first request (its handshake
held to 10 s) and shared by every request after. The server closes an idle one; the next request
makes a new one. A request whose stream could not be opened on a connection the server had just
closed is sent once on a new connection over HTTP/3: nothing of it had been sent. Each process
has one UDP socket per address family.

### Proxies

blaze's TCP client does not go through a proxy yet (#1495). HTTP proxies carry TCP only, so
with `HTTPS_PROXY` (or `https_proxy`, `ALL_PROXY`, `all_proxy`) set, `"auto"` keeps every host
the proxy is for on TCP, and `"require"` is an error naming the host and the proxy (its
credentials removed). A host `NO_PROXY` exempts (`*`, the host, or a domain it is under) is
decided as if no proxy were set.

## Spans

Every wait of a request opens a span (target `blaze`): `http request` (method, host, protocol:
`h2`, `http/1.1` or `h3`, source, status) around each request of a chain of redirects;
`dns https record` (host, the `h3` port found or -1, the outcome) and its per-host turn;
`dns addresses`, `http3 connection lock`, `http3 connect`, `http3 open stream`, `http3 answer`
and `http3 body` (host, port, source); `http3 connection` while one is open.

## Measuring

`harness/netbench.py` compares HTTP/2 and HTTP/3 through this client: crates.io index files and
downloads and small GETs to a Cloudflare host, cold and warm (docs/harness.md, "netbench.py").
