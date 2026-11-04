//! WebTransport endpoint for the ix.dev/harness page, the same way ix.dev/explore reaches `blaze
//! explore`: ix.dev serves the page, and the browser dials this daemon on 127.0.0.1 straight from
//! the visitor's machine, trusting our self-signed certificate by its SHA-256 hash.
//!
//! The wire is generic over *objects* (Durable-Object style): every agent PTY, run and run stage is
//! one, with an id, kind, name, parent, append-only streams and RPC methods. The page is a
//! subscriber. Until the daemon has its own object index this maps onto `daemon::Session` (PTYs)
//! and `runs` (runs and stages); the op names stay when the backend swaps.
//!
//! # The link
//!
//! `https://ix.dev/harness#<host>:<port>/<token>/<certhash>`. The fragment never reaches ix.dev;
//! the page takes several links separated by `,` (one per daemon: this Mac, VMs, tailnet machines). `token` is 32 lowercase hex chars, fresh each daemon start; `certhash` is the
//! SHA-256 of the certificate DER as unpadded base64url (43 chars). Written to
//! `~/.harness/daemon.json` as `wt_link` (with `wt_port`, `wt_certhash`, `wt_cert_not_after_unix_s`).
//!
//! # Certificate
//!
//! ECDSA P-256, self-signed, valid 13 days (Chrome's `serverCertificateHashes` allows at most 14).
//! After 10 days a new one is loaded for new sessions; open sessions keep theirs and get a `cert`
//! event so the page can rewrite its link to the new hash.
//!
//! # Session
//!
//! One WebTransport session at `https://<host>:<port>/harness/v1`. The endpoint binds
//! HARNESS_WT_BIND (default 127.0.0.1; e.g. a tailnet address or 0.0.0.0 to be reachable from other
//! machines) on HARNESS_WT_PORT (default 4748). The link names HARNESS_WT_HOST, else the bind
//! address, else 127.0.0.1 when bound to every interface. Origins allowed: https://ix.dev, https://*.ix.dev, http(s)://localhost:*,
//! http(s)://127.0.0.1:*. The token is checked once per session, by `hello`; until then every
//! other stream gets `{"error":"bad_token"}`, and a wrong token closes the session (reason
//! `bad_token`).
//!
//! # Framing (both directions, every stream)
//!
//! ```text
//! frame = kind:u8  len:u32 big-endian  payload:[len]u8      (len <= 16 MiB)
//! kind 1 = JSON   payload is one UTF-8 JSON object
//! kind 2 = BYTES  payload is raw stream bytes
//! ```
//!
//! # Objects
//!
//! ```text
//! {"id":"s3","kind":"claude"|"codex"|"shell","name":title-or-kind,"parent":"s1"|null,
//!  "state":"running"|"exited","exit":code|null,"streams":["pty"],
//!  "methods":["input","resize","kill","prompt","interrupt"],"info":{...daemon session info...}}
//! {"id":"r9","kind":"run","name":cmd,"parent":"r4"|"s3"|null,"state":"running"|"exited"|"killed"|"lost",
//!  "exit":code|null,"streams":["stdout","stderr"],"methods":["kill"],"info":{...run meta...}}
//! {"id":"r9.2","kind":"stage","name":stage-text,"parent":"r9","state":...,"exit":...,
//!  "streams":["stdout","stderr"],"methods":[],"info":{...stage...}}
//! ```
//!
//! # Control stream: the first bidi stream the page opens. Requests carry `req`, answers echo it.
//!
//! ```text
//! -> {"req":0,"op":"hello","token":T}
//! <- {"req":0,"daemon":"harnessd","schema":1,"certhash":H,"cert_not_after_unix_s":N,
//!     "device":{"id":"<16 hex, stable, ~/.harness/device.json>","hostname":..,"os":..,"arch":..}}
//! -> {"req":n,"op":"objects"}                         <- {"req":n,"objects":[...]}
//! -> {"req":n,"op":"spawn","kind":"claude"|"codex"|"shell","name"?,"parent"?:"s1","prompt"?,"cols"?,"rows"?}
//! -> {"req":n,"op":"spawn","kind":"run","cmd":"...","cwd"?,"parent"?}
//!                                                     <- {"req":n,"id":"s4"|"r10"}
//! -> {"req":n,"op":"rpc","id":"s4","method":M,"args":{...}}   <- {"req":n,"ok":true,...}
//!      input {data:string}  resize {cols,rows}  kill {}  prompt {text, submit?=true}  interrupt {}
//! any failure                                         <- {"req":n,"error":"..."}
//! ```
//!
//! # Updates: one unidirectional stream the daemon opens after hello
//!
//! ```text
//! <- {"upsert":{object}}     an object appeared or changed (every object once at the start)
//! <- {"remove":"s4"}         it is gone
//! <- {"cert":{"certhash":H,"cert_not_after_unix_s":N}}   after a rotation
//! ```
//!
//! # Data: one bidi stream per subscription
//!
//! ```text
//! -> {"op":"subscribe","id":"s3","stream":"pty","from"?:offset,"cols"?,"rows"?}
//! <- JSON  header, or {"error":"..."} and the stream ends
//! ```
//! `pty`: the header is `{"cols","rows","object"}`; cols/rows in the request resize the PTY first
//! (the browser is the primary client). Then BYTES: a snapshot redrawing the current screen, then
//! live output (another snapshot if this reader fell behind); `from` is ignored, the snapshot is
//! the replay. JSON `{"exit":code}` when the process ends. Page -> daemon: BYTES are `input`,
//! JSON `{"resize":{"cols","rows"}}` is `resize`.
//!
//! `stdout`/`stderr` (runs, stages): the header is `{"object","from"}`, then BYTES from byte
//! offset `from` (default 0) as they are written, then JSON `{"end":true,"exit":code}`.
//!
//! Closing either side unsubscribes; the object keeps running.
use crate::daemon::{Daemon, Ev};
use crate::{paths, runs};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, watch};
use wtransport::{Endpoint, Identity, ServerConfig};

const KIND_JSON: u8 = 1;
const KIND_BYTES: u8 = 2;
const MAX_FRAME: usize = 16 << 20;
const PATH: &str = "/harness/v1";
const VALID_DAYS: u32 = 13;
const ROTATE_AFTER: Duration = Duration::from_secs(10 * 24 * 3600);
const SCHEMA: u32 = 1;
/// How many recent runs show as objects.
const RUNS_SHOWN: usize = 50;

#[derive(Clone, PartialEq)]
struct Cert {
    hash: String,
    not_after_unix_s: u64,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn make_identity(host: &str) -> Result<(Identity, Cert)> {
    // The page pins the hash, so the names are informational; list the ones it may dial.
    let identity = Identity::self_signed_builder()
        .subject_alt_names(["localhost", "127.0.0.1", host])
        .from_now_utc()
        .validity_days(VALID_DAYS)
        .build()
        .map_err(|e| anyhow!("self-signed certificate: {e}"))?;
    let leaf = identity.certificate_chain().as_slice().first().context("certificate chain is empty")?;
    let hash = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(leaf.hash().as_ref());
    Ok((identity, Cert { hash, not_after_unix_s: unix_now() + VALID_DAYS as u64 * 86_400 }))
}

fn server_config(bind: std::net::SocketAddr, identity: Identity) -> ServerConfig {
    ServerConfig::builder()
        .with_bind_address(bind)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .build()
}

fn random_token() -> Result<String> {
    let mut b = [0u8; 16];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Merge our fields into daemon.json, keeping whatever the daemon already wrote there.
fn write_info(host: &str, port: u16, token: &str, cert: &Cert) -> Result<String> {
    let path = paths::info();
    let mut v: Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}));
    let link = format!("https://ix.dev/harness#{host}:{port}/{token}/{}", cert.hash);
    v["wt_port"] = json!(port);
    v["wt_link"] = json!(link);
    v["wt_certhash"] = json!(cert.hash);
    v["wt_cert_not_after_unix_s"] = json!(cert.not_after_unix_s);
    std::fs::write(&path, v.to_string()).with_context(|| format!("write {}", path.display()))?;
    Ok(link)
}

fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(o) = origin else { return false };
    if o == "https://ix.dev" || (o.starts_with("https://") && o.ends_with(".ix.dev")) {
        return true;
    }
    let rest = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")).unwrap_or("");
    let host = rest.split(':').next().unwrap_or("");
    host == "localhost" || host == "127.0.0.1"
}

/// Start the endpoint; log and give up (the rest of the daemon keeps working) if it can't.
pub async fn serve(d: Arc<Daemon>) {
    if let Err(e) = serve_inner(d).await {
        eprintln!("webtransport: {e:#}");
    }
}

struct Shared {
    d: Arc<Daemon>,
    token: String,
    cert: watch::Receiver<Cert>,
    device: Value,
}

/// This machine as the page shows it: a random id kept in ~/.harness/device.json (so it survives
/// restarts and renames), the hostname, os and arch.
fn device() -> Result<Value> {
    let path = paths::home().join("device.json");
    let id = match std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(v) if v["id"].is_string() => v["id"].as_str().unwrap_or_default().to_string(),
        _ => {
            let id = random_token()?[..16].to_string();
            std::fs::write(&path, json!({ "id": id }).to_string()).with_context(|| format!("write {}", path.display()))?;
            id
        }
    };
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for its length; gethostname NUL-terminates within it on success.
    let hostname = if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    Ok(json!({ "id": id, "hostname": hostname, "os": std::env::consts::OS, "arch": std::env::consts::ARCH }))
}

async fn serve_inner(d: Arc<Daemon>) -> Result<()> {
    let port: u16 = std::env::var("HARNESS_WT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(4748);
    let ip: std::net::IpAddr = match std::env::var("HARNESS_WT_BIND") {
        Ok(b) => b.parse().with_context(|| format!("HARNESS_WT_BIND={b:?} is not an IP address"))?,
        Err(_) => [127, 0, 0, 1].into(),
    };
    let host = std::env::var("HARNESS_WT_HOST").unwrap_or_else(|_| {
        if ip.is_unspecified() { "127.0.0.1".into() } else if ip.is_ipv6() { format!("[{ip}]") } else { ip.to_string() }
    });
    let token = random_token()?;
    let (identity, cert) = make_identity(&host)?;
    let endpoint = Arc::new(Endpoint::server(server_config((ip, port).into(), identity)).with_context(|| format!("bind udp {ip}:{port}"))?);
    let bind = endpoint.local_addr()?;
    let port = bind.port();
    let link = write_info(&host, port, &token, &cert)?;
    eprintln!("harnessd webtransport {link}");
    let (cert_tx, cert_rx) = watch::channel(cert);
    let shared = Arc::new(Shared { d, token: token.clone(), cert: cert_rx, device: device()? });

    // Rotate by wall clock (a monotonic sleep stops while the Mac sleeps).
    {
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            let mut made = SystemTime::now();
            loop {
                tokio::time::sleep(Duration::from_secs(600)).await;
                if made.elapsed().unwrap_or_default() < ROTATE_AFTER {
                    continue;
                }
                let res = (|| {
                    let (identity, cert) = make_identity(&host)?;
                    endpoint.reload_config(server_config(bind, identity), false)?;
                    write_info(&host, port, &token, &cert)?;
                    Ok::<_, anyhow::Error>(cert)
                })();
                match res {
                    Ok(cert) => {
                        eprintln!("webtransport: rotated certificate, new hash {}", cert.hash);
                        made = SystemTime::now();
                        let _ = cert_tx.send(cert);
                    }
                    Err(e) => eprintln!("webtransport: certificate rotation failed: {e:#}"),
                }
            }
        });
    }

    loop {
        let incoming = endpoint.accept().await;
        let shared = shared.clone();
        tokio::spawn(async move {
            let request = match incoming.await {
                Ok(r) => r,
                Err(e) => return eprintln!("webtransport: handshake: {e}"),
            };
            if request.path().split('?').next() != Some(PATH) {
                return request.not_found().await;
            }
            if !origin_allowed(request.origin()) {
                eprintln!("webtransport: refused origin {:?}", request.origin());
                return request.forbidden().await;
            }
            let conn = match request.accept().await {
                Ok(c) => c,
                Err(e) => return eprintln!("webtransport: accept: {e}"),
            };
            let authed = Arc::new(AtomicBool::new(false));
            loop {
                let Ok((send, recv)) = conn.accept_bi().await else { return };
                let (shared, conn, authed) = (shared.clone(), conn.clone(), authed.clone());
                tokio::spawn(async move {
                    if let Err(e) = stream(&shared, &conn, &authed, send, recv).await {
                        if e.to_string() == "bad_token" {
                            conn.close(1u32.into(), b"bad_token");
                        } else if !e.to_string().contains("connection lost") {
                            // A page closing its tab ends every stream with "connection lost"; that's normal.
                            eprintln!("webtransport stream: {e:#}");
                        }
                    }
                });
            }
        });
    }
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        bail!("frame of {len} bytes is over the {MAX_FRAME} limit");
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Some((head[0], payload)))
}

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

async fn write_json<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    w.write_all(&frame(KIND_JSON, v.to_string().as_bytes())).await?;
    Ok(())
}

fn token_ok(given: &str, want: &str) -> bool {
    given.len() == want.len() && given.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

async fn stream(
    sh: &Arc<Shared>, conn: &wtransport::Connection, authed: &AtomicBool, mut send: wtransport::SendStream,
    mut recv: wtransport::RecvStream,
) -> Result<()> {
    let Some((KIND_JSON, first)) = read_frame(&mut recv).await? else { bail!("stream did not open with a JSON frame") };
    let first: Value = serde_json::from_slice(&first).context("first frame is not JSON")?;
    let op = first["op"].as_str().unwrap_or("");
    if op == "hello" {
        if !token_ok(first["token"].as_str().unwrap_or(""), &sh.token) {
            write_json(&mut send, &json!({ "req": first["req"], "error": "bad_token" })).await?;
            let _ = send.finish().await;
            bail!("bad_token");
        }
        authed.store(true, Ordering::Release);
        return control(sh.clone(), conn.clone(), first, send, recv).await;
    }
    if !authed.load(Ordering::Acquire) {
        write_json(&mut send, &json!({ "error": "bad_token" })).await?;
        let _ = send.finish().await;
        return Ok(());
    }
    match op {
        "subscribe" => subscribe(&sh.d, first, send, recv).await,
        other => {
            write_json(&mut send, &json!({ "error": format!("unknown op {other:?} (want hello or subscribe)") })).await?;
            let _ = send.finish().await;
            Ok(())
        }
    }
}

// ---- objects over the current backend ----

fn session_object(info: &Value) -> Value {
    let title = info["title"].as_str().unwrap_or("");
    let kind = info["kind"].as_str().unwrap_or("");
    json!({
        "id": format!("s{}", info["id"]),
        "kind": kind,
        "name": if title.is_empty() { kind } else { title },
        "parent": info["parent"].as_u64().map(|p| format!("s{p}")),
        "state": if info["exit"].is_null() { "running" } else { "exited" },
        "exit": info["exit"],
        "streams": ["pty"],
        "methods": ["input", "resize", "kill", "prompt", "interrupt"],
        "info": info,
    })
}

fn run_objects(m: &Value, out: &mut Vec<Value>) {
    let id = m["id"].as_str().unwrap_or("").to_string();
    let parent = m["parent"].as_str().map(String::from).or_else(|| m["session"].as_u64().map(|s| format!("s{s}")));
    out.push(json!({
        "id": id, "kind": "run", "name": m["cmd"], "parent": parent, "state": m["status"],
        "exit": m["exit_code"], "streams": ["stdout", "stderr"], "methods": ["kill"], "info": m,
    }));
    // A plain single command is its own one stage; list stages only when there is a pipeline or sequence.
    let stages = m["stages"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if stages.len() > 1 {
        for (k, s) in stages.iter().enumerate() {
            out.push(json!({
                "id": format!("{id}.{k}"), "kind": "stage", "name": s["text"], "parent": id,
                "state": s["status"], "exit": s["exit_code"], "streams": ["stdout", "stderr"], "methods": [], "info": s,
            }));
        }
    }
}

/// Every object, oldest first. Blocking (reads run metadata from disk).
fn objects(d: &Daemon) -> Vec<Value> {
    let mut out: Vec<Value> = d.list().iter().map(session_object).collect();
    if let Ok(v) = runs::handle(runs::RunReq::Runs { session: None, limit: Some(RUNS_SHOWN) }) {
        for m in v["runs"].as_array().into_iter().flatten().rev() {
            run_objects(m, &mut out);
        }
    }
    out
}

fn session_id(id: &str) -> Result<u64> {
    id.strip_prefix('s').and_then(|n| n.parse().ok()).with_context(|| format!("{id:?} is not a PTY object"))
}

/// (run id, stage) for "r9" or "r9.2".
fn run_id(id: &str) -> Result<(String, Option<usize>)> {
    if !id.starts_with('r') {
        bail!("{id:?} is not a run object");
    }
    Ok(match id.split_once('.') {
        Some((r, k)) => (r.to_string(), Some(k.parse().with_context(|| format!("bad stage in {id:?}"))?)),
        None => (id.to_string(), None),
    })
}

fn handle(d: &Arc<Daemon>, req: &Value) -> Result<Value> {
    let s = |k: &str| req[k].as_str().with_context(|| format!("missing {k:?}"));
    Ok(match req["op"].as_str().unwrap_or("") {
        "objects" => json!({ "objects": objects(d) }),
        "spawn" => match s("kind")? {
            "run" => {
                let parent = req["parent"].as_str();
                let session = parent.and_then(|p| session_id(p).ok());
                let parent_run = parent.filter(|p| p.starts_with('r')).map(String::from);
                let v = runs::handle(runs::RunReq::Run {
                    cmd: s("cmd")?.to_string(), cwd: req["cwd"].as_str().map(String::from), env: BTreeMap::new(),
                    clear_env: false, session, parent: parent_run, stdin: None,
                })?;
                json!({ "id": v["id"] })
            }
            kind => {
                let parent = req["parent"].as_str().map(session_id).transpose()?;
                let cols = req["cols"].as_u64().map_or(120, |c| c as u16);
                let rows = req["rows"].as_u64().map_or(40, |r| r as u16);
                let id = d.spawn(kind, &[], parent, req["cwd"].as_str().map(String::from), req["prompt"].as_str(), cols, rows)?;
                json!({ "id": format!("s{id}") })
            }
        },
        "rpc" => rpc(d, s("id")?, s("method")?, &req["args"])?,
        other => bail!("unknown op {other:?}"),
    })
}

fn rpc(d: &Arc<Daemon>, id: &str, method: &str, args: &Value) -> Result<Value> {
    if id.starts_with('r') {
        let (run, stage) = run_id(id)?;
        return match (method, stage) {
            ("kill" | "interrupt", None) => runs::handle(runs::RunReq::RunKill { id: run }),
            _ => bail!("{id} has no method {method:?}"),
        };
    }
    let s = d.get(session_id(id)?)?;
    match method {
        "input" => s.write(args["data"].as_str().context("input needs args.data")?.as_bytes())?,
        "resize" => {
            let (c, r) = (args["cols"].as_u64(), args["rows"].as_u64());
            s.resize(c.context("resize needs args.cols")? as u16, r.context("resize needs args.rows")? as u16)?;
        }
        "kill" => s.kill(),
        "interrupt" => s.write(b"\x03")?,
        "prompt" => {
            // As the socket API's send (per-kind submit strategy in inject.rs).
            let text = args["text"].as_str().context("prompt needs args.text")?;
            crate::inject::send(&s, text, args["submit"].as_bool().unwrap_or(true))?;
        }
        _ => bail!("{id} has no method {method:?}"),
    }
    Ok(json!({ "ok": true }))
}

// ---- control stream and the update stream ----

async fn control(
    sh: Arc<Shared>, conn: wtransport::Connection, hello: Value, mut send: wtransport::SendStream,
    mut recv: wtransport::RecvStream,
) -> Result<()> {
    let mut cert_rx = sh.cert.clone();
    let cert = cert_rx.borrow_and_update().clone();
    write_json(
        &mut send,
        &json!({
            "req": hello["req"], "daemon": "harnessd", "schema": SCHEMA, "certhash": cert.hash,
            "cert_not_after_unix_s": cert.not_after_unix_s, "device": sh.device,
        }),
    )
    .await?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            write_json(&mut send, &v).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let updates = tokio::spawn(push_updates(sh.d.clone(), conn, cert_rx));
    let res = async {
        while let Some((kind, payload)) = read_frame(&mut recv).await? {
            if kind != KIND_JSON {
                continue;
            }
            let req: Value = match serde_json::from_slice(&payload) {
                Ok(v) => v,
                Err(e) => {
                    tx.send(json!({ "error": format!("bad request: {e}") }))?;
                    continue;
                }
            };
            let (d, tx) = (sh.d.clone(), tx.clone());
            tokio::spawn(async move {
                let id = req["req"].clone();
                let out = match tokio::task::spawn_blocking(move || handle(&d, &req)).await {
                    Ok(Ok(mut v)) => {
                        v["req"] = id;
                        v
                    }
                    Ok(Err(e)) => json!({ "req": id, "error": format!("{e:#}") }),
                    Err(e) => json!({ "req": id, "error": e.to_string() }),
                };
                let _ = tx.send(out);
            });
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    updates.abort();
    drop(tx);
    let _ = writer.await;
    res
}

/// Every object once, then upserts/removes as they change (polled every 500 ms), and rotations.
async fn push_updates(d: Arc<Daemon>, conn: wtransport::Connection, mut cert_rx: watch::Receiver<Cert>) -> Result<()> {
    let mut send = conn.open_uni().await?.await?;
    let mut known: BTreeMap<String, Value> = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let d = d.clone();
                let now = tokio::task::spawn_blocking(move || objects(&d)).await?;
                let mut seen = BTreeMap::new();
                for o in now {
                    let id = o["id"].as_str().unwrap_or("").to_string();
                    if known.get(&id) != Some(&o) {
                        write_json(&mut send, &json!({ "upsert": o })).await?;
                    }
                    seen.insert(id, o);
                }
                for id in known.keys().filter(|id| !seen.contains_key(*id)) {
                    write_json(&mut send, &json!({ "remove": id })).await?;
                }
                known = seen;
            }
            changed = cert_rx.changed() => {
                if changed.is_err() { return Ok(()); }
                let c = cert_rx.borrow_and_update().clone();
                write_json(&mut send, &json!({ "cert": { "certhash": c.hash, "cert_not_after_unix_s": c.not_after_unix_s } })).await?;
            }
        }
    }
}

// ---- subscriptions ----

async fn subscribe(d: &Arc<Daemon>, req: Value, mut send: wtransport::SendStream, recv: wtransport::RecvStream) -> Result<()> {
    let id = req["id"].as_str().unwrap_or("").to_string();
    let stream = req["stream"].as_str().unwrap_or("").to_string();
    let res: Result<()> = match stream.as_str() {
        "pty" => match session_id(&id).and_then(|n| d.get(n)) {
            Ok(s) => return subscribe_pty(d, s, req, send, recv).await,
            Err(e) => Err(e),
        },
        "stdout" | "stderr" => match run_id(&id) {
            Ok((run, stage)) => return subscribe_run(d, id, run, stage, stream, req["from"].as_u64().unwrap_or(0), send).await,
            Err(e) => Err(e),
        },
        other => Err(anyhow!("no stream {other:?}; objects have pty, stdout or stderr")),
    };
    if let Err(e) = res {
        write_json(&mut send, &json!({ "error": format!("{e:#}") })).await?;
        let _ = send.finish().await;
    }
    Ok(())
}

async fn subscribe_pty(
    d: &Arc<Daemon>, s: Arc<crate::daemon::Session>, req: Value, mut send: wtransport::SendStream,
    mut recv: wtransport::RecvStream,
) -> Result<()> {
    if let (Some(c), Some(r)) = (req["cols"].as_u64(), req["rows"].as_u64()) {
        s.resize(c as u16, r as u16)?;
    }
    *d.last.lock().unwrap() = Some(s.id);
    let (cols, rows) = s.size();
    write_json(&mut send, &json!({ "cols": cols, "rows": rows, "object": session_object(&s.info()) })).await?;
    let (snap, mut rx) = s.snapshot_and_subscribe();
    send.write_all(&frame(KIND_BYTES, &snap)).await?;
    let exited = *s.exit.lock().unwrap();
    s.attached.fetch_add(1, Ordering::Relaxed);
    let out = {
        let s = s.clone();
        async move {
            if let Some(code) = exited {
                write_json(&mut send, &json!({ "exit": code })).await?;
                let _ = send.finish().await;
                // Nothing more will come; stay subscribed until the page closes the stream.
                return std::future::pending().await;
            }
            loop {
                match rx.recv().await {
                    Ok(Ev::Data(b)) => send.write_all(&frame(KIND_BYTES, &b)).await?,
                    Ok(Ev::Exit(code)) => {
                        write_json(&mut send, &json!({ "exit": code })).await?;
                        let _ = send.finish().await;
                        return Ok::<_, anyhow::Error>(());
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let (snap, new_rx) = s.snapshot_and_subscribe();
                        rx = new_rx;
                        send.write_all(&frame(KIND_BYTES, &snap)).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    };
    let input = {
        let s = s.clone();
        async move {
            while let Some((kind, payload)) = read_frame(&mut recv).await? {
                match kind {
                    KIND_BYTES => {
                        let s = s.clone();
                        tokio::task::spawn_blocking(move || s.write(&payload)).await??;
                    }
                    KIND_JSON => {
                        let v: Value = serde_json::from_slice(&payload).unwrap_or_default();
                        if let (Some(c), Some(r)) = (v["resize"]["cols"].as_u64(), v["resize"]["rows"].as_u64()) {
                            s.resize(c as u16, r as u16)?;
                        }
                    }
                    _ => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        }
    };
    let res = tokio::select! {
        r = input => r,
        r = out => r,
    };
    s.attached.fetch_sub(1, Ordering::Relaxed);
    res
}

/// Tail a run's (or stage's) stdout/stderr from `from` until the run is done and read to the end.
async fn subscribe_run(
    d: &Arc<Daemon>, id: String, run: String, stage: Option<usize>, stream: String, from: u64, mut send: wtransport::SendStream,
) -> Result<()> {
    let obj = {
        let d = d.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || objects(&d).into_iter().find(|o| o["id"] == id.as_str())).await?
    };
    let Some(obj) = obj else {
        write_json(&mut send, &json!({ "error": format!("no object {id}") })).await?;
        let _ = send.finish().await;
        return Ok(());
    };
    write_json(&mut send, &json!({ "object": obj, "from": from })).await?;
    let short = if stream == "stdout" { "out" } else { "err" }.to_string();
    let mut offset = from;
    loop {
        let req = runs::RunReq::RunRead { id: run.clone(), stage, stream: short.clone(), offset, max_bytes: Some(1 << 20), wait_ms: 1000 };
        let v = tokio::task::spawn_blocking(move || runs::handle(req)).await??;
        let data = v["data"].as_str().unwrap_or("");
        if !data.is_empty() {
            send.write_all(&frame(KIND_BYTES, data.as_bytes())).await?;
        }
        offset = v["next_offset"].as_u64().unwrap_or(offset);
        if v["done"].as_bool() == Some(true) && v["eof"].as_bool() == Some(true) {
            break;
        }
    }
    let r = run.clone();
    let st = tokio::task::spawn_blocking(move || runs::handle(runs::RunReq::RunStatus { id: r })).await??;
    let exit = match stage {
        Some(k) => st["stages"][k]["exit_code"].clone(),
        None => st["exit_code"].clone(),
    };
    write_json(&mut send, &json!({ "end": true, "exit": exit })).await?;
    let _ = send.finish().await;
    Ok(())
}
