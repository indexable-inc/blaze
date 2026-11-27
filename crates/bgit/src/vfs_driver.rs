//! Native macOS lazy VFS driver over `blaze-git`'s sans-IO [`LazyVfs`] and [`Nfs3Conn`].
//!
//! Mounts any `.bgit` (or `.git` via `open_or_infect`) repository onto a local directory using
//! macOS's built-in loopback NFSv3 client (`/sbin/mount_nfs`) — zero kexts, zero macFUSE, zero
//! root privileges, zero third-party deps, `#![forbid(unsafe_code)]`.
//!
//! - **O(1) mount**: 0 working-tree files written to disk.
//! - **Lazy hydration**: trees parsed on first directory visit, blob sizes served from mmap `BGIX`
//!   in O(log N) with zero Zstd decompression, blob bytes read only when opened.
//! - **Read-write CoW overlay**: edits/creates/deletes/renames stay in the overlay and publish a
//!   CRC-32-verified `BGOV` v1 manifest so `bgit status <mountpoint>` runs in **O(dirty)** (< 1 ms).

use std::{
    env, fs,
    io::{self, Read as _, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_git::{
    Head, PromptStatus, RepoState, ShortOid, StatusOpts, Truncated, UntrackedMode, WorktreeKind,
    nfs3::Nfs3Conn,
    storage::{ObjectStore, RefStore, RefTarget},
    vfs::{LazyVfs, VfsOpts, decode_overlay_status, encode_overlay_status},
};
use blaze_git_fetch::{Kind as ObjectKind, Oid, store::Commit};
use blaze_own::{Alloc, Vec as OwnVec};
use bstd::cli::Matches;

use crate::{
    IoStreams,
    disk::{BgitDiskStore, open_or_infect},
    resolve_bgit_binary, resolve_path,
};

/// Parsed mount registry record stored in `~/.bgit/vfs/<id>/mount.info`.
#[derive(Clone, Debug)]
pub(crate) struct MountInfo {
    pub(crate) state_dir: PathBuf,
    pub(crate) mountpoint: PathBuf,
    pub(crate) repo_dir: PathBuf,
    pub(crate) pid: u32,
    pub(crate) port: u16,
}

fn vfs_registry_root() -> PathBuf {
    if let Ok(home) = env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".bgit/vfs");
    }
    env::temp_dir().join("bgit-vfs")
}

fn mount_id_for(canonical_mountpoint: &Path) -> String {
    let bytes = canonical_mountpoint.as_os_str().as_encoded_bytes();
    let c1 = blaze_deflate::crc32(bytes);
    // Mix with FNV-1a 64-bit so distinct paths never collide in the registry dir.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{c1:08x}-{h:016x}")
}

fn is_actively_mounted(mountpoint: &Path) -> bool {
    let Ok(m_meta) = fs::symlink_metadata(mountpoint) else {
        return false;
    };
    let Some(parent) = mountpoint.parent() else {
        return false;
    };
    let Ok(p_meta) = fs::symlink_metadata(parent) else {
        return false;
    };
    m_meta.dev() != p_meta.dev()
}

fn read_mount_info(state_dir: &Path) -> Option<MountInfo> {
    let text = fs::read_to_string(state_dir.join("mount.info")).ok()?;
    let mut lines = text.lines();
    let mountpoint = PathBuf::from(lines.next()?);
    let repo_dir = PathBuf::from(lines.next()?);
    let pid: u32 = lines.next()?.parse().ok()?;
    let port: u16 = lines.next()?.parse().ok()?;
    Some(MountInfo { state_dir: state_dir.to_path_buf(), mountpoint, repo_dir, pid, port })
}

fn write_mount_info(info: &MountInfo) -> io::Result<()> {
    fs::create_dir_all(&info.state_dir)?;
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        info.mountpoint.display(),
        info.repo_dir.display(),
        info.pid,
        info.port
    );
    let tmp = info.state_dir.join("mount.info.tmp");
    fs::write(&tmp, body)?;
    fs::rename(tmp, info.state_dir.join("mount.info"))
}

fn list_active_mounts() -> Vec<MountInfo> {
    let root = vfs_registry_root();
    let Ok(rd) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut active = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if let Some(info) = read_mount_info(&p)
            && is_actively_mounted(&info.mountpoint)
        {
            active.push(info);
        }
    }
    active.sort_unstable_by(|a, b| a.mountpoint.cmp(&b.mountpoint));
    active
}

/// Fast-path for `bgit status`: if `start` lies inside an active lazy VFS mount, read its
/// `BGOV` v1 overlay manifest directly in **O(dirty)** time (< 1 ms, 0 directory walks!).
pub(crate) fn try_vfs_mount_status<'a>(
    start: &Path,
    opts: StatusOpts,
    alloc: Alloc<'a>,
) -> io::Result<Option<PromptStatus<'a>>> {
    let Ok(canon) = fs::canonicalize(start) else {
        return Ok(None);
    };
    for info in list_active_mounts() {
        if canon == info.mountpoint || canon.starts_with(&info.mountpoint) {
            let overlay_path = info.state_dir.join("overlay.bgov");
            let bytes = fs::read(&overlay_path)?;
            let manifest = decode_overlay_status(&bytes, alloc).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("bad BGOV overlay: {e}"))
            })?;
            let head = match manifest.branch {
                Some(name) => Head::Branch {
                    name,
                    oid: manifest.head_oid,
                    short: manifest.head_oid.map(|o| ShortOid::from_oid(o, opts.short_oid_len)),
                },
                None => {
                    let oid = manifest.head_oid.unwrap_or(Oid([0u8; 20]));
                    Head::Detached { oid, short: ShortOid::from_oid(oid, opts.short_oid_len) }
                }
            };
            let mut counts = manifest.counts;
            let mut entries = manifest.entries;
            if opts.untracked == UntrackedMode::Off {
                counts.untracked = 0;
                let mut filtered = OwnVec::with_capacity_in(entries.len(), alloc);
                for e in entries {
                    if !e.is_untracked() {
                        filtered.push(e);
                    }
                }
                entries = filtered;
            }
            return Ok(Some(PromptStatus {
                head,
                tag: None,
                state: RepoState::Clean,
                upstream: None,
                counts,
                worktree: WorktreeKind::Normal,
                truncated: Truncated::default(),
                changed_samples: OwnVec::new_in(alloc),
                entries,
            }));
        }
    }
    Ok(None)
}

/// Public entry point for `bgit mount` (supports background mount, `--foreground`, `--ro`, `--list`).
pub(crate) fn cmd_mount<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    if matches.flag("list") {
        let active = list_active_mounts();
        if active.is_empty() {
            writeln!(io.out, "No active bgit VFS mounts.")?;
            return Ok(());
        }
        for info in active {
            let dirty = fs::read(info.state_dir.join("overlay.bgov"))
                .ok()
                .and_then(|b| decode_overlay_status(&b, alloc).ok())
                .map_or(0, |m| m.entries.len());
            writeln!(
                io.out,
                "{}\t<- {}\t(port={}, pid={}, dirty={})",
                info.mountpoint.display(),
                info.repo_dir.display(),
                info.port,
                info.pid,
                dirty
            )?;
        }
        return Ok(());
    }

    let pos = matches.positionals();
    if pos.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: bgit mount <repo> <mountpoint> [rev] [--ro] [--foreground]",
        ));
    }
    let repo_dir = resolve_path(cwd, pos[0]);
    let mountpoint = resolve_path(cwd, pos[1]);
    let rev = pos.get(2).copied().unwrap_or("HEAD");
    let read_only = matches.flag("ro") || matches.flag("read-only");
    let foreground = matches.flag("foreground");

    if foreground {
        serve_vfs_mount(&repo_dir, &mountpoint, rev, read_only, None, alloc)
    } else {
        spawn_background_mount(&repo_dir, &mountpoint, rev, read_only, io.out)
    }
}

/// Spawn a detached background VFS daemon and wait (bounded 10 s) until `mount_nfs` completes.
pub(crate) fn spawn_background_mount(
    repo_dir: &Path,
    mountpoint: &Path,
    rev: &str,
    read_only: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    fs::create_dir_all(mountpoint)?;
    let canon_repo = fs::canonicalize(repo_dir)?;
    let canon_mnt = fs::canonicalize(mountpoint)?;
    if is_actively_mounted(&canon_mnt) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is already mounted", canon_mnt.display()),
        ));
    }

    let state_dir = vfs_registry_root().join(mount_id_for(&canon_mnt));
    fs::create_dir_all(&state_dir)?;
    let ready_file = state_dir.join("ready");
    let _ = fs::remove_file(&ready_file);
    let _ = fs::remove_file(state_dir.join("stop"));

    let exe = resolve_bgit_binary();
    let mut cmd = Command::new(exe);
    cmd.arg("__vfs-serve")
        .arg(&canon_repo)
        .arg(&canon_mnt)
        .arg(rev)
        .arg("--ready-file")
        .arg(&ready_file);
    if read_only {
        cmd.arg("--ro");
    }
    let log_file = fs::File::create(state_dir.join("vfs.log"))?;
    let log_err = log_file.try_clone()?;
    cmd.stdin(Stdio::null()).stdout(Stdio::from(log_file)).stderr(Stdio::from(log_err));

    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = fs::read_to_string(&ready_file)
            && !text.is_empty()
        {
            let _ = fs::remove_file(&ready_file);
            if let Some(rest) = text.strip_prefix("ok\t") {
                let parts: Vec<&str> = rest.trim().split('\t').collect();
                let port = parts.first().copied().unwrap_or("?");
                let pid = parts.get(1).copied().unwrap_or("?");
                let head_desc = parts.get(2).copied().unwrap_or("HEAD");
                writeln!(
                    out,
                    "Mounted {} ({}) lazily at {} via NFSv3 (127.0.0.1:{}, pid {}, 0 files written to disk)\n  \
                     status:  bgit status {}   (O(dirty), <1ms)\n  \
                     unmount: bgit unmount {}",
                    canon_repo.display(),
                    head_desc,
                    canon_mnt.display(),
                    port,
                    pid,
                    canon_mnt.display(),
                    canon_mnt.display(),
                )?;
                return Ok(());
            }
            let msg = text.strip_prefix("err\t").unwrap_or(&text).trim();
            return Err(io::Error::other(format!("vfs mount failed: {msg}")));
        }
        if let Some(status) = child.try_wait()? {
            let log = fs::read_to_string(state_dir.join("vfs.log")).unwrap_or_default();
            return Err(io::Error::other(format!(
                "vfs daemon exited early ({status}): {}",
                log.trim()
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting 10s for NFSv3 mount_nfs to complete",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Internal entry point (`bgit __vfs-serve`) used by background mounts.
pub(crate) fn cmd_internal_vfs_serve<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let repo_dir = resolve_path(cwd, matches.positional(0).expect("required REPO"));
    let mountpoint = resolve_path(cwd, matches.positional(1).expect("required MOUNTPOINT"));
    let rev = matches.positional(2).expect("required REV");
    let read_only = matches.flag("ro");
    let ready_file = matches.opt("ready-file").map(|p| resolve_path(cwd, p));
    serve_vfs_mount(&repo_dir, &mountpoint, rev, read_only, ready_file.as_deref(), alloc)
}

/// Public entry point for `bgit unmount <mountpoint> [--force]`.
pub(crate) fn cmd_unmount<'a>(
    matches: &Matches<'_, Alloc<'a>>,
    cwd: &Path,
    io: &mut IoStreams<'_>,
) -> io::Result<()> {
    let mnt = resolve_path(cwd, matches.positional(0).expect("required MOUNTPOINT"));
    cmd_unmount_path(&mnt, matches.flag("force"), io.out)
}

pub(crate) fn cmd_unmount_path(mnt: &Path, force: bool, writer: &mut dyn Write) -> io::Result<()> {
    let canon_mnt = fs::canonicalize(mnt).unwrap_or_else(|_| mnt.to_path_buf());
    let state_dir = vfs_registry_root().join(mount_id_for(&canon_mnt));

    let mut cmd = Command::new("/sbin/umount");
    if force {
        cmd.arg("-f");
    }
    cmd.arg(&canon_mnt);
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "umount {} failed: {}",
            canon_mnt.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    // Signal background daemon to exit cleanly and clean up registry metadata.
    if state_dir.is_dir() {
        let _ = fs::write(state_dir.join("stop"), b"1");
        let _ = fs::remove_file(state_dir.join("mount.info"));
        let _ = fs::remove_file(state_dir.join("overlay.bgov"));
    }
    writeln!(writer, "Unmounted {}", canon_mnt.display())?;
    Ok(())
}

fn now_unix_sec() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(1_700_000_000, |d| d.as_secs() as u32)
}

struct SharedVfsState<'a> {
    vfs: LazyVfs<'a>,
    store: BgitDiskStore<'a>,
    branch_name: Option<String>,
    head_oid: Option<Oid>,
    state_dir: PathBuf,
    last_published_gen: u64,
}

impl<'a> SharedVfsState<'a> {
    fn publish_overlay_if_changed(&mut self, alloc: Alloc<'a>) {
        let generation = self.vfs.overlay_generation();
        if generation == self.last_published_gen {
            return;
        }
        if let Ok((counts, entries)) = self.vfs.dirty_status(UntrackedMode::Normal, &self.store, alloc) {
            let mut buf = OwnVec::new_in(alloc);
            encode_overlay_status(
                generation,
                self.branch_name.as_deref(),
                self.head_oid,
                counts,
                &entries,
                &mut buf,
            );
            let tmp = self.state_dir.join("overlay.bgov.tmp");
            if fs::write(&tmp, buf.as_slice()).is_ok() {
                let _ = fs::rename(&tmp, self.state_dir.join("overlay.bgov"));
                self.last_published_gen = generation;
            }
        }
    }
}

fn resolve_rev_for_vfs<'a>(
    store: &BgitDiskStore<'a>,
    rev: &str,
    alloc: Alloc<'a>,
) -> io::Result<(Option<String>, Option<Oid>, Option<Oid>)> {
    let mut branch_name = None;
    let oid = if rev == "HEAD" {
        if let Some(RefTarget::Symbolic(sym)) = store
            .read_head(alloc)
            .map_err(|e| io::Error::other(format!("{e}")))?
        {
            let b = sym.as_str().strip_prefix("refs/heads/").unwrap_or(sym.as_str());
            branch_name = Some(b.to_string());
        }
        store.resolve_ref("HEAD").map_err(|e| io::Error::other(format!("{e}")))?
    } else if rev.len() == 40
        && let Some(o) = Oid::parse_hex(rev.as_bytes())
    {
        Some(o)
    } else {
        let mut found = None;
        for cand in [rev.to_string(), format!("refs/heads/{rev}"), format!("refs/tags/{rev}")] {
            if let Ok(Some(o)) = store.resolve_ref(&cand) {
                if cand.starts_with("refs/heads/") {
                    branch_name = Some(rev.to_string());
                }
                found = Some(o);
                break;
            }
        }
        if found.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown revision: {rev}"),
            ));
        }
        found
    };

    let tree_oid = if let Some(commit_oid) = oid {
        let mut raw = OwnVec::new_in(alloc);
        let kind = store
            .read_object(&commit_oid, &mut raw)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        match kind {
            Some(ObjectKind::Commit) => {
                let c = Commit::parse(&raw)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
                Some(c.tree)
            }
            Some(ObjectKind::Tree) => Some(commit_oid),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("revision {rev} is {other:?}, expected commit or tree"),
                ));
            }
        }
    } else {
        None
    };

    Ok((branch_name, oid, tree_oid))
}

fn serve_vfs_mount<'a>(
    repo_dir: &Path,
    mountpoint: &Path,
    rev: &str,
    read_only: bool,
    ready_file: Option<&Path>,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    fs::create_dir_all(mountpoint)?;
    let canon_repo = fs::canonicalize(repo_dir)?;
    let canon_mnt = fs::canonicalize(mountpoint)?;
    let mnt_meta = fs::metadata(&canon_mnt)?;
    let uid = mnt_meta.uid();
    let gid = mnt_meta.gid();

    let store = open_or_infect(&canon_repo, alloc)?;
    let (branch_name, head_oid, root_tree) = resolve_rev_for_vfs(&store, rev, alloc)?;

    let vfs_opts =
        VfsOpts { read_only, uid, gid, base_mtime_sec: now_unix_sec(), ..VfsOpts::DEFAULT };
    let vfs = LazyVfs::new(root_tree, vfs_opts, alloc);

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;

    let state_dir = vfs_registry_root().join(mount_id_for(&canon_mnt));
    fs::create_dir_all(&state_dir)?;
    let _ = fs::remove_file(state_dir.join("stop"));

    let info = MountInfo {
        state_dir: state_dir.clone(),
        mountpoint: canon_mnt.clone(),
        repo_dir: canon_repo.clone(),
        pid: std::process::id(),
        port,
    };
    write_mount_info(&info)?;

    // Write initial clean `overlay.bgov` (gen = 0).
    let mut init_buf = OwnVec::new_in(alloc);
    encode_overlay_status(
        0,
        branch_name.as_deref(),
        head_oid,
        blaze_git::StatusCounts::default(),
        &[],
        &mut init_buf,
    );
    fs::write(state_dir.join("overlay.bgov"), init_buf.as_slice())?;

    let head_desc = match (&branch_name, head_oid) {
        (Some(b), Some(o)) => format!("{b}@{}", ShortOid::from_oid(o, 7)),
        (Some(b), None) => format!("{b} (unborn)"),
        (None, Some(o)) => format!("{}", ShortOid::from_oid(o, 7)),
        (None, None) => "empty".to_string(),
    };

    let mut state = SharedVfsState {
        vfs,
        store,
        branch_name,
        head_oid,
        state_dir: state_dir.clone(),
        last_published_gen: 0,
    };
    let shutdown = AtomicBool::new(false);
    let mounted = AtomicBool::new(false);
    let pid = std::process::id();

    thread::scope(|scope| {
        // Trigger OS `/sbin/mount_nfs` concurrently while this thread serves MOUNTv3 + NFSv3.
        let mnt_ref = &canon_mnt;
        let head_desc_ref = &head_desc;
        let shutdown_ref = &shutdown;
        let mounted_ref = &mounted;
        scope.spawn(move || {
            let mut opts = format!(
                "nolocks,nobrowse,vers=3,tcp,rsize=131072,wsize=131072,port={port},mountport={port},actimeo=1"
            );
            if read_only {
                opts.push_str(",rdonly");
            }
            let res = Command::new("/sbin/mount_nfs")
                .arg("-o")
                .arg(&opts)
                .arg("127.0.0.1:/")
                .arg(mnt_ref)
                .output();
            match res {
                Ok(out) if out.status.success() => {
                    mounted_ref.store(true, Ordering::Release);
                    if let Some(rf) = ready_file {
                        let tmp = rf.with_extension("tmp");
                        let _ = fs::write(&tmp, format!("ok\t{port}\t{pid}\t{head_desc_ref}\n"));
                        let _ = fs::rename(tmp, rf);
                    } else {
                        eprintln!(
                            "bgit VFS mounted at {} ({head_desc_ref}, 127.0.0.1:{port}, 0 files materialized). Run `bgit unmount {}` to stop.",
                            mnt_ref.display(),
                            mnt_ref.display()
                        );
                    }
                }
                Ok(out) => {
                    let err_msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if let Some(rf) = ready_file {
                        let _ = fs::write(rf, format!("err\t{err_msg}\n"));
                    } else {
                        eprintln!("bgit mount_nfs failed: {err_msg}");
                    }
                    shutdown_ref.store(true, Ordering::Release);
                }
                Err(e) => {
                    if let Some(rf) = ready_file {
                        let _ = fs::write(rf, format!("err\t{e}\n"));
                    } else {
                        eprintln!("bgit failed to spawn /sbin/mount_nfs: {e}");
                    }
                    shutdown_ref.store(true, Ordering::Release);
                }
            }
        });

        // Lock-free single-threaded non-blocking event loop over all active NFSv3 TCP clients.
        let mut conns: Vec<(TcpStream, Nfs3Conn<'a>)> = Vec::new();
        let mut rx_chunk = vec![0u8; 65_536];
        let mut tx_out = OwnVec::with_capacity_in(65_536, alloc);
        let mut last_check = Instant::now();
        let mut idle_spins = 0u32;

        while !shutdown.load(Ordering::Acquire) {
            let mut did_work = false;

            // Accept any pending TCP connections (MOUNTv3 + NFSv3).
            loop {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        let _ = stream.set_nodelay(true);
                        let _ = stream.set_nonblocking(true);
                        conns.push((stream, Nfs3Conn::new(alloc)));
                        did_work = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }

            // Poll all active connections.
            let now = now_unix_sec();
            let mut i = 0;
            while i < conns.len() {
                let mut drop_conn = false;
                loop {
                    match conns[i].0.read(&mut rx_chunk) {
                        Ok(0) => {
                            drop_conn = true;
                            break;
                        }
                        Ok(n) => {
                            did_work = true;
                            conns[i].1.feed_bytes(&rx_chunk[..n]);
                            tx_out.clear();
                            if conns[i].1.poll(&mut state.vfs, &state.store, now, &mut tx_out).is_err()
                            {
                                drop_conn = true;
                                break;
                            }
                            if !tx_out.is_empty()
                                && write_all_nonblocking(&mut conns[i].0, tx_out.as_slice()).is_err()
                            {
                                drop_conn = true;
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            drop_conn = true;
                            break;
                        }
                    }
                }
                if drop_conn {
                    conns.swap_remove(i);
                } else {
                    i += 1;
                }
            }

            if did_work {
                state.publish_overlay_if_changed(alloc);
                idle_spins = 0;
            } else {
                idle_spins = idle_spins.saturating_add(1);
                if idle_spins < 20 {
                    thread::sleep(Duration::from_micros(100));
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }

            if last_check.elapsed() >= Duration::from_millis(250) {
                last_check = Instant::now();
                if state_dir.join("stop").exists() {
                    shutdown.store(true, Ordering::Release);
                } else if mounted.load(Ordering::Acquire) && !is_actively_mounted(&canon_mnt) {
                    shutdown.store(true, Ordering::Release);
                }
            }
        }
    });

    let _ = fs::remove_file(state_dir.join("mount.info"));
    let _ = fs::remove_file(state_dir.join("overlay.bgov"));
    let _ = fs::remove_file(state_dir.join("stop"));
    Ok(())
}

fn write_all_nonblocking(stream: &mut TcpStream, mut data: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !data.is_empty() {
        match stream.write(data) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "tcp write zero")),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "tcp write timeout"));
                }
                thread::sleep(Duration::from_micros(50));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
