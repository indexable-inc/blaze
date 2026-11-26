//! Thin native network & stdio drivers for `bgit` server AND client modes over `blaze-git::wire`.
//!
//! - **Server**:
//!   - [`serve_http`]: Smart HTTP/1.1 server speaking Git Protocol v2 (`upload-pack`) and v1
//!     (`receive-pack`). Compatible directly with `git clone`, `git fetch`, `git push`, `git ls-remote`.
//!   - [`serve_stdio`]: Bidirectional stdio transport (`upload-pack` v2 or `receive-pack` v1).
//! - **Client**:
//!   - [`http_ls_remote`], [`http_clone`], [`http_fetch`], [`http_push`]: Smart HTTP/1.1 client
//!     talking to `bgit serve --http` or any standard Git Smart HTTP server!

use std::{
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::Path,
};

use blaze_git::{
    pack_write::PackWriteOpts,
    storage::{ObjectStore, RefStore, RefTarget, RefUpdate, ZERO_OID},
    wire::{
        HttpParse, ReceivePackServer, Transport, UploadPackServer, WireClient, WireLimits,
        handle_smart_http, parse_http_request,
    },
};
use blaze_git_fetch::{Kind as ObjectKind, Oid, store::Commit};
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};

use crate::disk::{BgitDiskStore, materialize_tree, open_or_infect};

/// Serves `repo_root` (`.bgit`, auto-infecting `.git` if needed) over Git Smart HTTP on `bind_addr`.
pub(crate) fn serve_http<'a>(bind_addr: &str, repo_root: &Path, alloc: Alloc<'a>) -> io::Result<()> {
    let mut store = open_or_infect(repo_root, alloc)?;
    let listener = TcpListener::bind(bind_addr)?;
    let local = listener.local_addr()?;
    println!("bgit-http listening on http://{local}/repo.git (objects={})", store.object_count());
    let _ = io::stdout().flush();

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        if let Err(e) = handle_http_conn(&mut stream, &mut store, alloc) {
            eprintln!("request error: {e}");
        }
    }
    Ok(())
}

fn handle_http_conn<'a>(
    stream: &mut TcpStream,
    store: &mut BgitDiskStore<'a>,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16_384];
    let req = loop {
        match parse_http_request(&buf, 256 << 20, alloc) {
            Ok(HttpParse::Complete { req, .. }) => break req,
            Ok(HttpParse::Incomplete) => {
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    return Ok(());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => {
                let msg = format!("HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\nConnection: close\r\n\r\nbad request\n");
                let _ = stream.write_all(msg.as_bytes());
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{e}")));
            }
        }
    };

    let mut resp = OwnVec::new_in(alloc);
    handle_smart_http(
        &req,
        store,
        WireLimits::SHIPPED,
        PackWriteOpts::FAST,
        alloc,
        &mut resp,
    )
    .map_err(|e| io::Error::other(format!("{e}")))?;

    // Persist any received objects & ref updates atomically before sending HTTP 200!
    store.flush()?;
    if req.target.contains("receive-pack")
        && let Some(repo_dir) = store.bgit_dir.parent()
    {
        let _ = crate::worktree_driver::sync_all_worktrees(repo_dir, None, false, false, false, alloc);
    }
    stream.write_all(&resp)?;
    stream.flush()?;
    Ok(())
}

/// Serves `upload-pack` (Protocol v2) or `receive-pack` (v1) over stdin/stdout.
pub(crate) fn serve_stdio<'a>(service: &str, repo_root: &Path, alloc: Alloc<'a>) -> io::Result<()> {
    let mut store = open_or_infect(repo_root, alloc)?;
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    match service {
        "upload-pack" | "git-upload-pack" => {
            let mut advert = OwnVec::new_in(alloc);
            UploadPackServer::advertise(Transport::Stream, &mut advert)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            stdout.write_all(&advert)?;
            stdout.flush()?;

            // Read pkt-line requests in a loop until client closes or sends flush.
            loop {
                let Some(req_bytes) = read_pkt_request(&mut stdin)? else {
                    break;
                };
                if req_bytes.is_empty() {
                    break;
                }
                let mut resp = OwnVec::new_in(alloc);
                UploadPackServer::handle_request(
                    &req_bytes,
                    &store,
                    WireLimits::SHIPPED,
                    PackWriteOpts::FAST,
                    alloc,
                    &mut resp,
                )
                .map_err(|e| io::Error::other(format!("{e}")))?;
                stdout.write_all(&resp)?;
                stdout.flush()?;
            }
            Ok(())
        }
        "receive-pack" | "git-receive-pack" => {
            let mut advert = OwnVec::new_in(alloc);
            ReceivePackServer::advertise(Transport::Stream, &store, alloc, &mut advert)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            stdout.write_all(&advert)?;
            stdout.flush()?;

            let mut req_all = Vec::new();
            stdin.read_to_end(&mut req_all)?;
            if req_all.is_empty() || req_all == b"0000" {
                return Ok(());
            }
            let mut resp = OwnVec::new_in(alloc);
            ReceivePackServer::handle_push(
                &req_all,
                &mut store,
                WireLimits::SHIPPED,
                alloc,
                &mut resp,
            )
            .map_err(|e| io::Error::other(format!("{e}")))?;
            store.flush()?;
            let _ = crate::worktree_driver::sync_all_worktrees(
                repo_root,
                None,
                false,
                false,
                false,
                alloc,
            );
            stdout.write_all(&resp)?;
            stdout.flush()?;
            Ok(())
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown stdio service `{other}` (use upload-pack or receive-pack)"),
        )),
    }
}

fn read_pkt_request(r: &mut impl io::Read) -> io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut hdr = [0u8; 4];
    loop {
        match r.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(if out.is_empty() { None } else { Some(out) });
            }
            Err(e) => return Err(e),
        }
        out.extend_from_slice(&hdr);
        let hex_str = core::str::from_utf8(&hdr)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid pkt header"))?;
        let len = usize::from_str_radix(hex_str, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid pkt hex"))?;
        if len == 0 {
            return Ok(Some(out));
        }
        if len == 1 || len == 2 {
            continue;
        }
        if len < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid pkt len"));
        }
        let prev = out.len();
        out.resize(prev + (len - 4), 0);
        r.read_exact(&mut out[prev..])?;
    }
}

// ---------------------------------------------------------------------------
// Smart HTTP/1.1 Client
// ---------------------------------------------------------------------------

struct ParsedHttpUrl<'u> {
    host_port: &'u str,
    base_path: &'u str,
}

fn parse_http_url(url: &str) -> io::Result<ParsedHttpUrl<'_>> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("only http:// URLs are supported directly by bgit client (got {url})"),
        )
    })?;
    let (host_port, base_path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], rest[idx..].trim_end_matches('/')),
        None => (rest, ""),
    };
    Ok(ParsedHttpUrl { host_port, base_path })
}

fn http_exchange(
    host_port: &str,
    method: &str,
    path_and_query: &str,
    content_type: Option<&str>,
    git_v2: bool,
    body: &[u8],
) -> io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(host_port)?;
    let mut req = Vec::with_capacity(256 + body.len());
    write!(req, "{method} {path_and_query} HTTP/1.1\r\nHost: {host_port}\r\nUser-Agent: blaze-git/1\r\nConnection: close\r\n")?;
    if git_v2 {
        req.extend_from_slice(b"Git-Protocol: version=2\r\n");
    }
    if let Some(ct) = content_type {
        write!(req, "Content-Type: {ct}\r\n")?;
    }
    if !body.is_empty() || method == "POST" {
        write!(req, "Content-Length: {}\r\n", body.len())?;
    }
    req.extend_from_slice(b"\r\n");
    req.extend_from_slice(body);
    stream.write_all(&req)?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let hdr_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP response"))?;
    let head = core::str::from_utf8(&raw[..hdr_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 HTTP response head"))?;
    let status_line = head.lines().next().unwrap_or("");
    if !status_line.contains(" 200 ") {
        return Err(io::Error::other(format!("HTTP server returned: {status_line}")));
    }
    let is_chunked = head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:") && l.to_ascii_lowercase().contains("chunked"));
    let body_raw = &raw[hdr_end + 4..];
    if !is_chunked {
        return Ok(body_raw.to_vec());
    }
    // Decode chunked HTTP response body.
    let mut decoded = Vec::new();
    let mut pos = 0usize;
    while pos < body_raw.len() {
        let Some(crlf) = body_raw[pos..].windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let len_s = core::str::from_utf8(&body_raw[pos..pos + crlf])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk len"))?;
        let n = usize::from_str_radix(len_s.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk hex"))?;
        pos += crlf + 2;
        if n == 0 {
            break;
        }
        if pos + n > body_raw.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated chunk"));
        }
        decoded.extend_from_slice(&body_raw[pos..pos + n]);
        pos += n + 2;
    }
    Ok(decoded)
}

/// Lists remote refs over Git Protocol v2 Smart HTTP.
pub(crate) fn http_ls_remote<'a>(url: &str, alloc: Alloc<'a>) -> io::Result<()> {
    let parsed = parse_http_url(url)?;
    let info_path = format!("{}/info/refs?service=git-upload-pack", parsed.base_path);
    let advert = http_exchange(parsed.host_port, "GET", &info_path, None, true, b"")?;
    blaze_git_fetch::check_advertisement(&advert)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad v2 advert: {e:?}")))?;

    let mut req = OwnVec::new_in(alloc);
    WireClient::encode_ls_refs(&[], &mut req).map_err(|e| io::Error::other(format!("{e}")))?;
    let up_path = format!("{}/git-upload-pack", parsed.base_path);
    let resp = http_exchange(
        parsed.host_port,
        "POST",
        &up_path,
        Some("application/x-git-upload-pack-request"),
        true,
        &req,
    )?;
    let refs = WireClient::parse_ls_refs(&resp, alloc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
    for r in refs {
        match r.oid {
            Some(oid) => println!("{oid}\t{}", r.name.as_str()),
            None => println!("unborn\t{}", r.name.as_str()),
        }
    }
    Ok(())
}

/// Clones a remote Git repository over Smart HTTP v2 directly into a native `.bgit` store
/// (and optionally checks out the working tree; pass `checkout = false` for lazy `--vfs` clones).
pub(crate) fn http_clone<'a>(
    url: &str,
    dest_dir: &Path,
    checkout: bool,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let parsed = parse_http_url(url)?;
    let info_path = format!("{}/info/refs?service=git-upload-pack", parsed.base_path);
    let advert = http_exchange(parsed.host_port, "GET", &info_path, None, true, b"")?;
    blaze_git_fetch::check_advertisement(&advert)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad v2 advert: {e:?}")))?;

    let mut ls_req = OwnVec::new_in(alloc);
    WireClient::encode_ls_refs(&[], &mut ls_req).map_err(|e| io::Error::other(format!("{e}")))?;
    let up_path = format!("{}/git-upload-pack", parsed.base_path);
    let ls_resp = http_exchange(
        parsed.host_port,
        "POST",
        &up_path,
        Some("application/x-git-upload-pack-request"),
        true,
        &ls_req,
    )?;
    let remote_refs = WireClient::parse_ls_refs(&ls_resp, alloc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;

    let mut wants = OwnVec::new_in(alloc);
    let mut head_sym: Option<String> = None;
    let mut head_oid: Option<Oid> = None;
    for r in &remote_refs {
        if r.name.as_str() == "HEAD" {
            head_oid = r.oid;
            head_sym = r.symref_target.as_ref().map(|s| s.as_str().to_string());
        }
        if let Some(oid) = r.oid
            && !wants.iter().any(|w| *w == oid)
        {
            wants.push(oid);
        }
    }

    let mut store = BgitDiskStore::init(dest_dir, alloc)?;
    let mut unpacked_count = 0u32;
    if !wants.is_empty() {
        let mut fetch_req = OwnVec::new_in(alloc);
        WireClient::encode_fetch(&wants, &[], None, &mut fetch_req)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        let fetch_resp = http_exchange(
            parsed.host_port,
            "POST",
            &up_path,
            Some("application/x-git-upload-pack-request"),
            true,
            &fetch_req,
        )?;
        let stats = WireClient::parse_fetch_and_unpack(
            &fetch_resp,
            &mut store,
            WireLimits::SHIPPED,
            alloc,
        )
        .map_err(|e| io::Error::other(format!("fetch unpack: {e}")))?;
        unpacked_count = stats.objects_unpacked;
    }

    // Apply refs & HEAD locally.
    let mut updates = OwnVec::new_in(alloc);
    for r in &remote_refs {
        if r.name.as_str() == "HEAD" {
            continue;
        }
        if let Some(oid) = r.oid {
            updates.push(RefUpdate {
                name: OwnString::from_str_in(r.name.as_str(), alloc),
                old: None,
                new: oid,
            });
        }
    }
    let _ = store
        .apply_ref_updates(&updates, false, alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    if let Some(sym) = head_sym {
        store
            .write_head(&RefTarget::Symbolic(OwnString::from_str_in(&sym, alloc)))
            .map_err(|e| io::Error::other(format!("{e}")))?;
    } else if let Some(oid) = head_oid {
        store
            .write_head(&RefTarget::Direct(oid))
            .map_err(|e| io::Error::other(format!("{e}")))?;
    }
    store.flush()?;

    // Materialize HEAD commit tree into `dest_dir` unless `--vfs` lazy checkout was requested.
    let mut checked_out = 0u32;
    if checkout && let Some(tip) = head_oid {
        let mut buf = OwnVec::new_in(alloc);
        if store
            .read_object(&tip, &mut buf)
            .map_err(|e| io::Error::other(format!("{e}")))?
            == Some(ObjectKind::Commit)
            && let Ok(c) = Commit::parse(&buf)
        {
            checked_out = materialize_tree(&store, c.tree, dest_dir, alloc)?;
        }
    }

    println!(
        "cloned {} into {} (objects={}, refs={}, files={})",
        url,
        dest_dir.display(),
        unpacked_count,
        remote_refs.len(),
        checked_out
    );
    Ok(())
}

/// Pushes a local ref from `repo_root` to a remote Git Smart HTTP server (`git-receive-pack`).
pub(crate) fn http_push<'a>(
    url: &str,
    repo_root: &Path,
    ref_name: &str,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let store = open_or_infect(repo_root, alloc)?;
    let full_ref = if ref_name.starts_with("refs/") {
        ref_name.to_string()
    } else {
        format!("refs/heads/{ref_name}")
    };
    let local_oid = store
        .resolve_ref(&full_ref)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("local ref `{full_ref}` not found")))?;

    let parsed = parse_http_url(url)?;
    let info_path = format!("{}/info/refs?service=git-receive-pack", parsed.base_path);
    let advert_bytes = http_exchange(parsed.host_port, "GET", &info_path, None, false, b"")?;
    let advert = WireClient::parse_receive_pack_advert(&advert_bytes, alloc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;

    let remote_old = advert
        .refs
        .iter()
        .find(|r| r.name.as_str() == full_ref)
        .and_then(|r| r.oid)
        .unwrap_or(ZERO_OID);

    if remote_old == local_oid {
        println!("Everything up-to-date ({full_ref} @ {local_oid})");
        return Ok(());
    }

    let updates = [RefUpdate {
        name: OwnString::from_str_in(&full_ref, alloc),
        old: Some(remote_old),
        new: local_oid,
    }];
    let mut push_req = OwnVec::new_in(alloc);
    WireClient::encode_push_request(
        &store,
        &advert,
        &updates,
        true,
        WireLimits::SHIPPED,
        PackWriteOpts::FAST,
        alloc,
        &mut push_req,
    )
    .map_err(|e| io::Error::other(format!("encode push: {e}")))?;

    let rp_path = format!("{}/git-receive-pack", parsed.base_path);
    let push_resp = http_exchange(
        parsed.host_port,
        "POST",
        &rp_path,
        Some("application/x-git-receive-pack-request"),
        false,
        &push_req,
    )?;
    let report = WireClient::parse_push_response(&push_resp, advert.sideband_64k, alloc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
    if !report.unpack_ok {
        return Err(io::Error::other(format!("remote unpack failed: {}", report.unpack_message.as_str())));
    }
    for r in &report.refs {
        if r.ok {
            println!("ok {} ({remote_old} -> {local_oid})", r.name.as_str());
        } else {
            return Err(io::Error::other(format!(
                "remote rejected {}: {}",
                r.name.as_str(),
                r.message.as_ref().map_or("rejected", |m| m.as_str())
            )));
        }
    }
    Ok(())
}
