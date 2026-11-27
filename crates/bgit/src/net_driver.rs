//! Thin native network & stdio drivers for `bgit` server AND client modes over `blaze-git::wire`.
//!
//! - **Server**:
//!   - [`serve_http`]: Smart HTTP/1.1 server speaking Git Protocol v2 (`upload-pack`) and v1
//!     (`receive-pack`). Compatible directly with `git clone`, `git fetch`, `git push`, `git ls-remote`.
//!   - [`serve_stdio`]: Bidirectional stdio transport (`upload-pack` v2 or `receive-pack` v1).
//! - **Client (Fastest GitHub & Git wire transports)**:
//!   - **HTTPS / TLS 1.3 + HTTP/2 (`blaze-h2`, ALPN `h2`) + persistent Keep-Alive** speaking Git
//!     Protocol v2 (`Git-Protocol: version=2`, `thin-pack`, `ofs-delta`, `--depth`, `--filter=blob:none`).
//!   - **Native SSH (`git@github.com:org/repo.git` & `ssh://...`)** speaking Git Protocol v2
//!     (`GIT_PROTOCOL=version=2`) over a single persistent stdio session.
//!   - **Plain HTTP/1.1** (`http://...`) for local `bgit serve --http` / LAN servers.

use std::{
    io::{self, Read as _, Write},
    net::{TcpListener, TcpStream},
    path::Path,
};

use blaze_git::{
    pack_write::PackWriteOpts,
    storage::{ObjectStore, RefStore, RefTarget, RefUpdate, ZERO_OID},
    wire::{
        HttpParse, ReceivePackServer, RemoteRef, Transport, UploadPackServer, WireClient,
        WireLimits, handle_smart_http, parse_http_request,
    },
};
use blaze_git_fetch::{Kind as ObjectKind, Oid, store::Commit};
use blaze_own::{AVec, Alloc, String as OwnString, Vec as OwnVec};

use crate::{
    disk::{BgitDiskStore, materialize_tree, open_or_infect},
    remote_transport::{RemoteUrl, SmartHttpSession, SshSession, parse_remote_url},
};

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
                let msg = "HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\nConnection: close\r\n\r\nbad request\n";
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
        let mut out = io::stdout();
        let mut err = io::stderr();
        let mut streams = crate::IoStreams { out: &mut out, err: &mut err };
        let _ = crate::worktree_driver::sync_all_worktrees(
            repo_dir,
            None,
            false,
            false,
            false,
            alloc,
            &mut streams,
        );
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
            let mut out_sink = io::sink();
            let mut err_sink = io::stderr();
            let mut streams = crate::IoStreams { out: &mut out_sink, err: &mut err_sink };
            let _ = crate::worktree_driver::sync_all_worktrees(
                repo_root,
                None,
                false,
                false,
                false,
                alloc,
                &mut streams,
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
// Unified Protocol v2 Client (`https://` HTTP/2+TLS1.3, `http://`, `ssh://`/`git@`)
// ---------------------------------------------------------------------------

/// Options for shallow (`--depth N`) and partial (`--filter=blob:none`) Git Protocol v2 fetches.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct FetchParams<'f> {
    pub depth: Option<u32>,
    pub filter: Option<&'f str>,
}

enum UploadPackClient {
    Http(SmartHttpSession),
    Ssh(SshSession),
}

impl UploadPackClient {
    fn open(url: &str) -> io::Result<Self> {
        match parse_remote_url(url)? {
            RemoteUrl::Http(target) => {
                // GitHub (`github.com`) natively accepts direct Protocol v2 `POST /git-upload-pack`
                // with `Git-Protocol: version=2` without a preliminary `GET /info/refs` round trip,
                // saving a full network RTT on every `ls-remote`, `clone`, `fetch`, and `pull`!
                let skip_info_refs = target.tls && target.host.eq_ignore_ascii_case("github.com");
                let mut sess = SmartHttpSession::new(target);
                if !skip_info_refs {
                    let advert = sess.exchange(
                        "GET",
                        "/info/refs?service=git-upload-pack",
                        None,
                        true,
                        b"",
                    )?;
                    blaze_git_fetch::check_advertisement(&advert).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("bad v2 advert: {e:?}"))
                    })?;
                }
                Ok(UploadPackClient::Http(sess))
            }
            RemoteUrl::Ssh(target) => {
                let mut ssh = SshSession::connect(&target, "git-upload-pack", true)?;
                let advert = ssh.read_pkt_until_flush()?;
                blaze_git_fetch::check_advertisement(&advert).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad ssh v2 advert: {e:?}"))
                })?;
                Ok(UploadPackClient::Ssh(ssh))
            }
        }
    }

    fn proto_label(&self) -> &'static str {
        match self {
            UploadPackClient::Http(s) => s.proto_label(),
            UploadPackClient::Ssh(_) => "ssh+git-v2",
        }
    }

    fn ls_refs<'a>(&mut self, prefixes: &[&str], alloc: Alloc<'a>) -> io::Result<AVec<'a, RemoteRef<'a>>> {
        let mut req = OwnVec::new_in(alloc);
        WireClient::encode_ls_refs(prefixes, &mut req)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        let resp = match self {
            UploadPackClient::Http(sess) => sess.exchange(
                "POST",
                "/git-upload-pack",
                Some("application/x-git-upload-pack-request"),
                true,
                &req,
            )?,
            UploadPackClient::Ssh(ssh) => {
                ssh.write_all(&req)?;
                ssh.read_pkt_until_flush()?
            }
        };
        WireClient::parse_ls_refs(&resp, alloc)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("ls-refs parse: {e}")))
    }

    fn fetch_into_store<'a>(
        &mut self,
        wants: &[Oid],
        haves: &[Oid],
        params: FetchParams<'_>,
        store: &mut BgitDiskStore<'a>,
        alloc: Alloc<'a>,
    ) -> io::Result<u32> {
        if wants.is_empty() {
            return Ok(0);
        }
        let mut fetch_req = OwnVec::new_in(alloc);
        // Use `thin-pack` when we already have base objects (`!haves.is_empty()`), or always on
        // remote fetches? `unpack_pack_into_store` resolves OFS/REF deltas against `store`, so
        // `thin-pack` works whenever `haves` exist in `store`!
        let thin_pack = !haves.is_empty();
        WireClient::encode_fetch_with_opts(
            wants,
            haves,
            params.depth,
            params.filter,
            thin_pack,
            &mut fetch_req,
        )
        .map_err(|e| io::Error::other(format!("{e}")))?;
        let resp = match self {
            UploadPackClient::Http(sess) => sess.exchange(
                "POST",
                "/git-upload-pack",
                Some("application/x-git-upload-pack-request"),
                true,
                &fetch_req,
            )?,
            UploadPackClient::Ssh(ssh) => {
                ssh.write_all(&fetch_req)?;
                ssh.read_pkt_until_flush()?
            }
        };
        let stats = WireClient::parse_fetch_and_unpack(&resp, store, WireLimits::SHIPPED, alloc)
            .map_err(|e| io::Error::other(format!("fetch unpack: {e}")))?;
        Ok(stats.objects_unpacked)
    }

    fn close(self) -> io::Result<()> {
        match self {
            UploadPackClient::Http(_) => Ok(()),
            UploadPackClient::Ssh(ssh) => ssh.finish(true),
        }
    }
}

/// Lists remote refs over Git Protocol v2 (`https://` HTTP/2+TLS1.3, `http://`, `ssh://`/`git@`).
pub(crate) fn remote_ls_remote<'a>(
    url: &str,
    alloc: Alloc<'a>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut client = UploadPackClient::open(url)?;
    let refs = client.ls_refs(&[], alloc)?;
    for r in &refs {
        match r.oid {
            Some(oid) => writeln!(out, "{oid}\t{}", r.name.as_str())?,
            None => writeln!(out, "unborn\t{}", r.name.as_str())?,
        }
    }
    client.close()
}

/// Clones a remote Git repository via Protocol v2 (HTTP/2+TLS 1.3 or SSH) into a native `.bgit`
/// store (and optionally checks out the working tree; pass `checkout = false` for lazy `--vfs`).
pub(crate) fn remote_clone<'a>(
    url: &str,
    dest_dir: &Path,
    checkout: bool,
    params: FetchParams<'_>,
    alloc: Alloc<'a>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut client = UploadPackClient::open(url)?;
    // On shallow/filtered clones from GitHub repos, fetch only HEAD + heads (skipping thousands
    // of `refs/pull/*` refs) and default to single-branch on `--depth` (matching `git clone --depth`)!
    let prefixes: &[&str] = if params.depth.is_some() {
        &["HEAD", "refs/heads/"]
    } else if params.filter.is_some() {
        &["HEAD", "refs/heads/", "refs/tags/"]
    } else {
        &[]
    };
    let remote_refs = client.ls_refs(prefixes, alloc)?;
    let proto = client.proto_label();

    let mut wants = OwnVec::new_in(alloc);
    let mut head_sym: Option<String> = None;
    let mut head_oid: Option<Oid> = None;
    for r in &remote_refs {
        let name = r.name.as_str();
        if name == "HEAD" {
            head_oid = r.oid;
            head_sym = r.symref_target.as_ref().map(|s| s.as_str().to_string());
        }
    }
    if params.depth.is_some() {
        if let Some(oid) = head_oid {
            wants.push(oid);
        }
    } else {
        for r in &remote_refs {
            let name = r.name.as_str();
            // Skip GitHub's internal `refs/pull/*` refs on clone (matching `git clone`!).
            if name.starts_with("refs/pull/") {
                continue;
            }
            if let Some(oid) = r.oid
                && !wants.iter().any(|w| *w == oid)
            {
                wants.push(oid);
            }
        }
    }

    let mut store = BgitDiskStore::init(dest_dir, alloc)?;
    let unpacked_count = client.fetch_into_store(&wants, &[], params, &mut store, alloc)?;
    client.close()?;

    // Apply local branch/tag refs & HEAD, plus `refs/remotes/origin/*` tracking refs!
    let mut updates = OwnVec::new_in(alloc);
    for r in &remote_refs {
        let name = r.name.as_str();
        if name == "HEAD" || name.starts_with("refs/pull/") {
            continue;
        }
        if params.depth.is_some() && head_sym.as_deref() != Some(name) {
            continue;
        }
        let Some(oid) = r.oid else {
            continue;
        };
        updates.push(RefUpdate {
            name: OwnString::from_str_in(name, alloc),
            old: None,
            new: oid,
        });
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            let remote_ref = format!("refs/remotes/origin/{branch}");
            updates.push(RefUpdate {
                name: OwnString::from_str_in(&remote_ref, alloc),
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

    // Materialize HEAD commit tree & populate `.bgit/index` unless `--vfs` or `--filter=blob:none`.
    let mut checked_out = 0u32;
    let is_blobless = params.filter.is_some_and(|f| f.contains("blob:none"));
    if checkout && !is_blobless && let Some(tip) = head_oid {
        let mut buf = OwnVec::new_in(alloc);
        if store
            .read_object(&tip, &mut buf)
            .map_err(|e| io::Error::other(format!("{e}")))?
            == Some(ObjectKind::Commit)
            && let Ok(c) = Commit::parse(&buf)
        {
            checked_out = materialize_tree(&store, c.tree, dest_dir, alloc)?;
            if let Ok(flat) = crate::index_io::flatten_tree(&store, c.tree, alloc) {
                let idx = crate::index_io::index_from_flat_tree(dest_dir, &flat);
                let _ = crate::index_io::write_index(dest_dir, &idx);
            }
        }
    }

    // Record `origin` URL in `.bgit/config` so `bgit fetch` / `bgit pull` / `bgit push` work immediately.
    let cfg_path = crate::index_io::GitConfig::config_path_for(dest_dir);
    let mut cfg = crate::index_io::GitConfig::load_file(&cfg_path);
    let _ = cfg.set("remote.origin.url", url);
    let _ = cfg.save_file(&cfg_path);

    writeln!(
        out,
        "cloned {} into {} (proto={}, objects={}, refs={}, files={})",
        url,
        dest_dir.display(),
        proto,
        unpacked_count,
        remote_refs.len(),
        checked_out
    )?;
    Ok(())
}

/// Fetches refs and missing objects over Protocol v2 (HTTP/2+TLS 1.3 or SSH) into `repo_root`,
/// updating `refs/remotes/<remote_name>/*` and mirroring to sibling `.git` if present.
pub(crate) fn remote_fetch<'a>(
    url: &str,
    remote_name: &str,
    repo_root: &Path,
    params: FetchParams<'_>,
    alloc: Alloc<'a>,
    out: &mut dyn Write,
) -> io::Result<u32> {
    let mut client = UploadPackClient::open(url)?;
    let remote_refs = client.ls_refs(&["refs/heads/", "refs/tags/"], alloc)?;
    let proto = client.proto_label();

    let mut store = open_or_infect(repo_root, alloc)?;
    let mut wants = OwnVec::new_in(alloc);
    let mut haves = OwnVec::new_in(alloc);

    let mut local_refs = OwnVec::new_in(alloc);
    store
        .list_refs("refs/", alloc, &mut local_refs)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    for lr in &local_refs {
        if let RefTarget::Direct(o) = lr.target
            && !haves.iter().any(|h| *h == o)
        {
            haves.push(o);
        }
    }

    for r in &remote_refs {
        if let Some(oid) = r.oid
            && !store.has_object(&oid)
            && !wants.iter().any(|w| *w == oid)
        {
            wants.push(oid);
        }
    }

    let unpacked_count = client.fetch_into_store(&wants, &haves, params, &mut store, alloc)?;
    client.close()?;

    let mut updates = OwnVec::new_in(alloc);
    let mut mirrored: Vec<(String, Oid)> = Vec::new();
    for r in &remote_refs {
        let Some(oid) = r.oid else {
            continue;
        };
        let local_ref = if let Some(branch) = r.name.as_str().strip_prefix("refs/heads/") {
            format!("refs/remotes/{remote_name}/{branch}")
        } else if r.name.as_str().starts_with("refs/tags/") {
            r.name.as_str().to_owned()
        } else {
            continue;
        };
        updates.push(RefUpdate {
            name: OwnString::from_str_in(&local_ref, alloc),
            old: None,
            new: oid,
        });
        mirrored.push((local_ref, oid));
    }
    let _ = store
        .apply_ref_updates(&updates, false, alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    crate::disk::mirror_overlay_objects_to_git(&store, repo_root, alloc);
    store.flush()?;
    for (ref_name, oid) in &mirrored {
        crate::disk::mirror_ref_to_git(repo_root, ref_name, *oid);
    }

    writeln!(
        out,
        "From {url} (proto={proto})\n * fetched {unpacked_count} object(s), updated {} ref(s)",
        mirrored.len()
    )?;
    Ok(unpacked_count)
}

/// Pushes a local ref from `repo_root` to a remote over HTTP/2+TLS 1.3, HTTP/1.1, or SSH (`git-receive-pack`).
pub(crate) fn remote_push<'a>(
    url: &str,
    repo_root: &Path,
    ref_name: &str,
    alloc: Alloc<'a>,
    out: &mut dyn Write,
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

    match parse_remote_url(url)? {
        RemoteUrl::Http(target) => {
            let mut sess = SmartHttpSession::new(target);
            let advert_bytes = sess.exchange(
                "GET",
                "/info/refs?service=git-receive-pack",
                None,
                false,
                b"",
            )?;
            let advert = WireClient::parse_receive_pack_advert(&advert_bytes, alloc)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;

            let remote_old = advert
                .refs
                .iter()
                .find(|r| r.name.as_str() == full_ref)
                .and_then(|r| r.oid)
                .unwrap_or(ZERO_OID);
            if remote_old == local_oid {
                writeln!(out, "Everything up-to-date ({full_ref} @ {local_oid})")?;
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

            let push_resp = sess.exchange(
                "POST",
                "/git-receive-pack",
                Some("application/x-git-receive-pack-request"),
                false,
                &push_req,
            )?;
            report_push_result(&push_resp, advert.sideband_64k, remote_old, local_oid, alloc, out)
        }
        RemoteUrl::Ssh(target) => {
            let mut ssh = SshSession::connect(&target, "git-receive-pack", false)?;
            let advert_bytes = ssh.read_pkt_until_flush()?;
            let advert = WireClient::parse_receive_pack_advert(&advert_bytes, alloc)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;

            let remote_old = advert
                .refs
                .iter()
                .find(|r| r.name.as_str() == full_ref)
                .and_then(|r| r.oid)
                .unwrap_or(ZERO_OID);
            if remote_old == local_oid {
                ssh.finish(true)?;
                writeln!(out, "Everything up-to-date ({full_ref} @ {local_oid})")?;
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

            ssh.write_all(&push_req)?;
            let push_resp = ssh.read_to_end()?;
            ssh.finish(false)?;
            report_push_result(&push_resp, advert.sideband_64k, remote_old, local_oid, alloc, out)
        }
    }
}

fn report_push_result<'a>(
    push_resp: &[u8],
    sideband_64k: bool,
    remote_old: Oid,
    local_oid: Oid,
    alloc: Alloc<'a>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let report = WireClient::parse_push_response(push_resp, sideband_64k, alloc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
    if !report.unpack_ok {
        return Err(io::Error::other(format!(
            "remote unpack failed: {}",
            report.unpack_message.as_str()
        )));
    }
    for r in &report.refs {
        if r.ok {
            writeln!(out, "ok {} ({remote_old} -> {local_oid})", r.name.as_str())?;
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
