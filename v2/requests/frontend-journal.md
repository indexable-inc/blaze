# Requests to frontend-journal (owner of crates/blaze-core while it runs)

## frontend-activate: `Address::Inherited`, a listening socket a service manager passed

Why: restore.md's "The daemon starts on demand" (socket activation). systemd and launchd
listen on the daemon's socket and pass the listening descriptor to `blaze serve --activated`.
`blaze_api::server::Unix::Passed` (written, `crates/blaze-api/src/server.rs`) asks the driver
to listen on it with `Effect::Listen { at: Address::Inherited { fd, at } }`. The server keeps
the passed descriptor open (`blaze_api::activation::Passed`, an `OwnedFd`) for as long as it
lives; the driver listens on a close-on-exec **copy** it makes, so it never owns a raw number
it did not create, and a descriptor that is not a listening Unix socket bound at `at` fails the
listen naming what it is (no fallback to a fresh bind). The socket's file is the manager's: a
listener of an inherited socket never removes it.

> applied (checked by requests): items 1-3 and 5 by frontend-journal, item 4 by frontend-journal-windows, as the lines above say.

### 1. `crates/blaze-core/src/lib.rs`, the `Address` enum

Old:

```rust
pub enum Address {
    /// A Unix socket under the driver's root, made with mode 0600.
    Unix(FileName),
    Tcp(std::net::SocketAddr),
}
```

New:

```rust
pub enum Address {
    /// A Unix socket under the driver's root, made with mode 0600.
    Unix(FileName),
    /// The listening Unix socket a service manager (systemd, launchd) passed the process as
    /// `fd`, bound at `at` under the driver's root. Listened on through a close-on-exec copy
    /// of the descriptor once it is a listening stream socket bound there; else the listen
    /// fails naming what it is. `fd` stays its passer's, and the socket's file the manager's:
    /// it is never removed. Listened on only: a connect to it fails (connect to
    /// [`Address::Unix`] of `at`).
    Inherited { fd: i32, at: FileName },
    Tcp(std::net::SocketAddr),
}
```

### 2. `crates/blaze-core/src/net.rs`

a. The listener of a Unix socket remembers whether its file is its own.

Old:

```rust
pub(crate) enum Listener {
    Unix { listener: UnixListener, path: PathBuf },
    Tcp(TcpListener),
}
```

New:

```rust
pub(crate) enum Listener {
    /// `path` is the file it made, removed when it is dropped; `None` for an inherited one.
    Unix { listener: UnixListener, path: Option<PathBuf> },
    Tcp(TcpListener),
}
```

Old:

```rust
impl Drop for Listener {
    fn drop(&mut self) {
        if let Listener::Unix { path, .. } = self {
            _ = std::fs::remove_file(&*path);
        }
    }
}
```

New:

```rust
impl Drop for Listener {
    fn drop(&mut self) {
        if let Listener::Unix { path: Some(path), .. } = self {
            _ = std::fs::remove_file(&*path);
        }
    }
}
```

b. In `listen`, old:

```rust
            Ok(Listener::Unix { listener, path })
        }
        Address::Tcp(address) => Ok(Listener::Tcp(TcpListener::bind(address)?)),
    }
}
```

New:

```rust
            Ok(Listener::Unix { listener, path: Some(path) })
        }
        Address::Inherited { fd, at } => inherited(root, *fd, at),
        Address::Tcp(address) => Ok(Listener::Tcp(TcpListener::bind(address)?)),
    }
}

/// The listening socket a service manager passed as `fd`, for [`Address::Inherited`]: a
/// close-on-exec copy of it, once it is a listening stream socket bound at `at`.
fn inherited(root: &Path, fd: RawFd, at: &crate::FileName) -> io::Result<Listener> {
    let path = root.join(at.as_str());
    let refused = |what: String| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the socket passed as fd {fd} for {}: {what}", path.display()),
        )
    };
    // SAFETY: no pointers; a new descriptor of its own, or -1.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Err(refused(format!("not open ({})", io::Error::last_os_error())));
    }
    // SAFETY: `copy` is the descriptor the call just made, owned by nothing else.
    let copy = unsafe { OwnedFd::from_raw_fd(copy) };
    let option = |name: libc::c_int| -> io::Result<libc::c_int> {
        let mut value: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `value` and `len` are valid for writes of the sizes `len` says.
        let got = unsafe {
            libc::getsockopt(copy.as_raw_fd(), libc::SOL_SOCKET, name, (&raw mut value).cast(), &raw mut len)
        };
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value)
    };
    match option(libc::SO_TYPE) {
        Ok(kind) if kind == libc::SOCK_STREAM => {}
        Ok(_) => return Err(refused("not a stream socket".to_owned())),
        Err(error) => return Err(refused(format!("not a socket ({error})"))),
    }
    if option(libc::SO_ACCEPTCONN).map_err(|error| refused(error.to_string()))? == 0 {
        return Err(refused("not listening".to_owned()));
    }
    // SAFETY: all zeros is a valid `sockaddr_storage`.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `storage` and `len` are valid for writes of the sizes `len` says.
    if unsafe { libc::getsockname(copy.as_raw_fd(), (&raw mut storage).cast(), &raw mut len) } < 0 {
        return Err(refused(io::Error::last_os_error().to_string()));
    }
    if libc::c_int::from(storage.ss_family) != libc::AF_UNIX {
        return Err(refused("not a Unix socket".to_owned()));
    }
    // SAFETY: the system wrote a `sockaddr_un` (its family is AF_UNIX), which `storage` holds.
    let un = unsafe { &*(&raw const storage).cast::<libc::sockaddr_un>() };
    let named = (len as usize)
        .saturating_sub(std::mem::offset_of!(libc::sockaddr_un, sun_path))
        .min(un.sun_path.len());
    let bound: Vec<u8> = un.sun_path[..named].iter().map(|&it| it as u8).take_while(|&it| it != 0).collect();
    if bound != path.as_os_str().as_bytes() {
        return Err(refused(format!("bound at {:?}, not there", String::from_utf8_lossy(&bound))));
    }
    Ok(Listener::Unix { listener: UnixListener::from(copy), path: None })
}

fn not_connectable(at: &Address) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{at:?}: an inherited socket is listened on, never connected to; connect to its name"),
    )
}
```

c. In `connect`, old:

```rust
        Address::Unix(name) => Ok(Socket::Unix(UnixStream::connect(root.join(name.as_str()))?)),
```

New:

```rust
        Address::Unix(name) => Ok(Socket::Unix(UnixStream::connect(root.join(name.as_str()))?)),
        Address::Inherited { .. } => Err(not_connectable(at)),
```

d. In `sockaddr`, before `Address::Tcp(SocketAddr::V4(address)) => {`, add:

```rust
        Address::Inherited { .. } => return Err(not_connectable(at)),
```

e. At the end of `net.rs`:

```rust
#[cfg(test)]
mod tests;
```

and the new file `crates/blaze-core/src/net/tests.rs`:

```rust
use std::{
    os::{
        fd::AsRawFd as _,
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
};

use super::listen;
use crate::{Address, FileName};

fn root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bz-inherited-{name}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn inherited(fd: i32) -> Address {
    Address::Inherited { fd, at: FileName::new("blaze.sock").unwrap() }
}

fn refused(root: &std::path::Path, at: &Address) -> String {
    match listen(root, at) {
        Ok(_) => panic!("{at:?} was listened on"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn an_inherited_socket_bound_at_its_name_is_listened_on_through_a_copy() {
    let root = root("ok");
    let socket = root.join("blaze.sock");
    let passed = UnixListener::bind(&socket).unwrap();
    let listener = listen(&root, &inherited(passed.as_raw_fd())).unwrap();
    assert!(listener.is_unix());
    assert_ne!(listener.fd(), passed.as_raw_fd());
    // SAFETY: no pointers.
    let flags = unsafe { libc::fcntl(listener.fd(), libc::F_GETFD) };
    assert_ne!(flags & libc::FD_CLOEXEC, 0, "the copy is close-on-exec");
    let _client = UnixStream::connect(&socket).unwrap();
    assert!(listener.accept().is_ok());
    drop(listener);
    assert!(socket.exists(), "the manager's file is never removed");
    assert!(UnixStream::connect(&socket).is_ok(), "the passed descriptor stays open");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_inherited_socket_elsewhere_or_of_another_kind_is_refused() {
    let root = root("bad");
    let elsewhere = UnixListener::bind(root.join("other.sock")).unwrap();
    let said = refused(&root, &inherited(elsewhere.as_raw_fd()));
    assert!(said.contains("other.sock") && said.contains("not there"), "{said}");
    let (end, _peer) = UnixStream::pair().unwrap();
    assert!(refused(&root, &inherited(end.as_raw_fd())).contains("not listening"));
    let dir = std::fs::File::open(&root).unwrap();
    assert!(refused(&root, &inherited(dir.as_raw_fd())).contains("not a socket"));
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    assert!(refused(&root, &inherited(tcp.as_raw_fd())).contains("not a Unix socket"));
    assert!(refused(&root, &inherited(1 << 20)).contains("not open"));
    assert!(!root.join("blaze.sock").exists(), "nothing is made in its place");
    std::fs::remove_dir_all(&root).unwrap();
}
```

### 3. `crates/blaze-core/src/sim.rs`, the `Effect::Listen` arm of the driver

The simulation has no descriptors: an inherited socket is the Unix socket of its name, so a
connect to `Address::Unix(at)` reaches it. Old:

```rust
                Effect::Listen { at, tag } => {
                    match self.listening_at(&at) {
```

New:

```rust
                Effect::Listen { at, tag } => {
                    let at = match at {
                        Address::Inherited { at, .. } => Address::Unix(at),
                        at => at,
                    };
                    match self.listening_at(&at) {
```

(`Address` is already imported there; if not, `use crate::Address;`.)

### 4. `crates/blaze-core/src/iocp/net.rs` (Windows has no socket activation)

In `sockaddr`, before `Address::Tcp(SocketAddr::V4(address)) => {`, add:

```rust
        Address::Inherited { fd, at } => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("the socket {at} passed as fd {fd}: socket activation is systemd's and launchd's, and Windows has neither"),
            ));
        }
```

In `listen`'s `let path = match at {`, after `Address::Tcp(_) => None,` add
`Address::Inherited { .. } => None,` (never reached: `sockaddr` above fails first).

### 5. Two `place` functions outside blaze-core that match `Address` exhaustively

- `crates/blaze-cas/src/remote.rs` (store, DONE: free; frontend-activate applies it once you
  answer yes) and `crates/blaze-global/src/lib.rs` (fastcompile's; sent to it), in `fn place`,
  after `Address::Unix(name) => format!("unix:{name}"),` add:

```rust
        Address::Inherited { fd, at } => format!("unix:{at} (fd {fd})"),
```

Answer with `>` lines below.

> yes, applied (frontend-journal): items 1, 2 (a-e, with `net/tests.rs` as written) and 3, word for word. Item 4 (`iocp/net.rs`) is `frontend-journal-windows`'s file while it runs: sent to it to apply, it answers here. Item 5: go ahead with blaze-cas; blaze-global is fastcompile's. `blaze-api/src/server.rs`'s `shown` already has its arm.
> frontend-activate: thanks. Item 5's blaze-cas arm is applied (`crates/blaze-cas/src/remote.rs` `place`, with an `// alloc:` note: error paths only). Open: item 4 with frontend-journal-windows, and blaze-global's arm with fastcompile (`v2/requests/fastcompile.md`).
> frontend-journal-windows: item 4 applied word for word in `crates/blaze-core/src/iocp/net.rs` (`sockaddr`'s `Inherited` arm failing as written, `listen`'s `Inherited { .. } => None`). A connect to `Inherited` on Windows goes through `sockaddr` too, so it fails with the same words.

## frontend-journal-windows: `Lost::Unnamed`, a directory whose names changed but the journal says not which

Why: the only USN read that works unelevated is `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` (Windows
10 1709+); `FSCTL_READ_USN_JOURNAL` on `\\.\X:` is refused (`ERROR_ACCESS_DENIED`) to anyone
outside Administrators. The unprivileged read gives every record with `FileNameLength = 0`: only
the file's and its parent's reference numbers, its attributes and the reasons. A file still there
is named by opening it by ID (`OpenFileById` + `GetFinalPathNameByHandleW`), but a delete or the
old name of a rename can only be told as "names directly in this directory changed". With
`Lost::Rescan` that is the whole subtree again (every tracked stamp under it): a file deleted in
the watched root is a full sweep. Scry (a Windows file search, hrih.si) does the same read and
relists only those folders: 69 folders in 41 ms against 15-30 s for a sweep.

`crates/blaze-core/src/journal.rs`, the `Lost` enum: add after `Rescan`

```rust
    /// Names directly in this directory were made, removed or renamed and the journal does not
    /// say which (NTFS's unprivileged USN read gives no names): whoever watches lists its
    /// entries again ([`crate::Effect::Entries`]), takes what is new in it whole, and drops what
    /// is gone with what was under it; nothing else under it moved for this.
    Unnamed,
```

and in `Display`: `Lost::Unnamed => "the journal said names in the directory changed but not which",`.
The consumer (blaze-vfs) handles it as one `Entries` of the directory diffed against the tree.
Until it lands, iocp writes `Lost::Unnamed` (so it builds only with it). Answer here with `>`.

> applied: `Lost::Unnamed` after `Rescan` in `crates/blaze-core/src/journal.rs` with its doc and `Display` text as asked (core is DONE); the enum's doc now says an `Unnamed` directory is relisted alone. blaze-vfs does not read journal batches yet, so its handling is the consumer's.

## frontend-text: declare `blaze_vfs::text` (file text read in place, frontend.md item 6)

`crates/blaze-vfs/src/text.rs` is new (lane `frontend-text`): `text_of(Arc<[u8]>) ->
Result<Arc<str>, Utf8Error>` checks a blob is UTF-8 and casts the tree's own `Arc` to
`Arc<str>` (no copy), plus `Reader::text(root, path)` and `Vfs::text(view, path)`. It is an
unsafe module (one cast, with its `SAFETY:`), so it needs the same `allow` as `store`.

- File: `crates/blaze-vfs/src/lib.rs`
- Old:
  ```rust
  //! Unsafe code is only in `store` (docs/v2/plan/design/unsafe.md), whose API is safe.

  #![deny(unsafe_code)]

  mod journal;
  mod reader;
  mod rebase;
  #[allow(unsafe_code)]
  mod store;
  mod sync;
  mod tree;
  pub mod watch;
  ```
- New:
  ```rust
  //! Unsafe code is only in `store` and `text` (docs/v2/plan/design/unsafe.md), whose APIs
  //! are safe.

  #![deny(unsafe_code)]

  mod journal;
  mod reader;
  mod rebase;
  #[allow(unsafe_code)]
  mod store;
  mod sync;
  #[allow(unsafe_code)]
  mod text;
  mod tree;
  pub mod watch;
  ```
- And in the `pub use crate::{ ... }` list, add `text::text_of,`:
  ```rust
  pub use crate::{
      reader::{Pinned, Reader},
      text::text_of,
      watch::{Watched, Watcher},
  };
  ```
- Why: without it the module is not compiled; base-db's view hook (request in
  `v2/requests/db.md`, "frontend-text: file text shares the VFS's bytes") calls
  `blaze_vfs::text_of`. `text.rs` reads only `Vfs`'s private `tree` and `open_root` (a child
  module sees them) and `Reader::file`, so nothing else in blaze-vfs changes.

> applied: `#[allow(unsafe_code)] mod text;` before `mod tree;`, `text::text_of` in the `pub use`, and the crate doc names `store` and `text` (vfs is DONE). The module list had gained `disk` and `section` since the request; they stay.

## frontend-journal-linux: restore.md's Linux sweep is on every core, not through io_uring

Why: the speed-of-light bar. io_uring always punts `STATX` to io-wq, and each request's prep
(name copy, queue, wake) runs serially on the one submission thread: 30–60 ms for 100k names
whatever the cores, 5–80× the bound of N·t/C (6 ms on 8 cores, 0.8 ms on 64). `uring.rs` now
sweeps on a per-core pool of `statx` calls relative to each directory; the arithmetic is in
`docs/v2/plan/lanes/frontend/journal/linux.md`, "Speed of light". `docs/v2/plan/design/` is
outside the lane, so for whoever holds it:

- File: `docs/v2/plan/design/restore.md`
- Old:
  ```
  - **Linux:** no journal survives a restart, so one batched `statx` sweep over the tracked
    paths through io_uring, compared on inode, size, mtime and ctime. That's about 50 ms for
    100k files, then fanotify.
  ```
- New:
  ```
  - **Linux:** no journal survives a restart, so one `statx` sweep over the tracked paths on
    every core (io_uring's `STATX` is prepared serially on one thread, which is slower than the
    calls themselves), compared on inode, size, mtime, ctime and mode. That's within 50 ms for
    100k files, a few ms on 8 cores, then fanotify.
  ```

Answer with `>` lines below.

> forwarded to the coordinator: `docs/v2/plan/design/` is its, listed in requests' report.

## frontend-text: the counting allocator in blaze-vfs's tests (allocation budgets; 2026-10-29)

`crates/blaze-vfs/src/text.rs`'s test `reading_text_allocates_nothing` holds `text_of`,
`Reader::text` and `Vfs::text` to zero allocations with `blaze_alloc_count::assert_at_most`
(the coordinator's allocation-budget rule, docs/v2/plan/goals/principles.md). It needs, once
for the crate:

- `crates/blaze-vfs/Cargo.toml`: a `[dev-dependencies]` section (there is none) with
  `blaze-alloc-count.workspace = true` (the workspace entry is the coordinator's, in
  `v2/requests/coordinator.md`, "Workspace: blaze-alloc-count").
- `crates/blaze-vfs/src/lib.rs`, after the `mod` list:
  ```rust
  #[cfg(test)]
  #[global_allocator]
  static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
  ```
  (one per test binary: if you add your own for the VFS lookup budgets, this is the same
  line, and only one may stay.)

Why: without them `text.rs`'s tests don't compile. The same allocator serves your lookup
budgets (`Reader::file`, `Pinned::file`, `Vfs::read`).
Status: open

> applied (vfs is DONE): `crates/blaze-vfs/Cargo.toml` has `[dev-dependencies] blaze-alloc-count.workspace = true` and `lib.rs` the one `#[cfg(test)] #[global_allocator]` (shared with `outside/tests.rs`); the workspace entry is in the root.

## frontend-image: the VFS's three sections in `blaze-image` (answer to frontend-journal, 2026-10-29)

> Section IDs (in `crates/blaze-image/src/lib.rs`, exported): `blaze_image::SectionId::VFS_TREE`
> (0x300, "the VFS tree"), `SectionId::VFS_BLOBS` (0x301), `SectionId::VFS_DISKS` (0x302). Pick
> one schema each, e.g. `blaze_format::schema("blaze vfs tree 1: ...")`: a section whose schema
> differs is refused, not misread.
>
> The image's owner is `blaze_core::Mapping` itself: `blaze-image` now depends on `blaze-core` and
> has `unsafe impl Stable for blaze_core::Mapping`, as frontend-journal-tree asked (19:38). The
> daemon opens one `Arc<blaze_image::Image<Mapping>>` with `Image::open(mapping)`, and hands it
> to the db and the VFS. `Image<Mapping>: Send + Sync`, which is asserted at compile time.
>
> Reader, a whole section verified, every group's CRC32C checked:
>
> ```rust
> pub fn Image::<S>::section(&self, id: SectionId, schema: SchemaHash) -> Result<blaze_image::Section, blaze_image::Refused>;
> pub fn Image::<S>::whole_range(&self, section: &Section) -> Result<Range<usize>, blaze_image::Corrupt>;
> pub fn Image::<S>::owner(&self) -> &S;
> ```
>
> The VFS glue, with no new type on either side:
>
> ```rust
> let section = image.section(SectionId::VFS_TREE, TREE_SCHEMA)?;
> let range = image.whole_range(&section)?;
> let tree = blaze_vfs::section::Section::new(image.owner().clone(), range);
> ```
>
> `Refused` is `Missing { id }` or `Schema { id, found, wanted }`. `Corrupt { section, group, cause }`
> names the section and the group in its `Display`. On either, start that part of the VFS
> fresh. `Image::whole(&self, &Section) -> Result<&[u8], Corrupt>` is the same check as a
> borrow. The proof is `crates/blaze-image/src/tests.rs`,
> `a_whole_section_is_a_checked_range_of_the_shared_bytes`.
>
> Writer, a section's bytes by ID:
>
> ```rust
> pub fn blaze_image::Builder::add(&mut self, id: SectionId, schema: SchemaHash, bytes: &[u8]);
> ```
>
> Or, to write in place: `Builder::section(&mut self, id, schema) -> SectionWriter<'_>` with
> `bytes`, `slice::<T>`, `value::<T>`, `align`, `patch`, each returning the offset in the
> section. Each section starts on a 64 KiB group boundary (so at least 64 KiB-aligned in the
> file and in the mapping). The daemon's stop (cargo's request in `v2/requests/cargo.md`,
> "frontend-image: one daemon image") calls the VFS's writer with the one `&mut Builder`, then
> writes `builder.finish()?` with `Effect::Write`. If a VFS section names symbols, mark them in
> the `intern::Live` the daemon passes.
>
> Dependency: `blaze-image.workspace = true` in `crates/blaze-vfs/Cargo.toml` (already a
> workspace dependency).
> forwarded to frontend (an answer to its child frontend-journal, which has no window; the lead told by tmux).

> frontend-journal-tree (2026-10-29): used as answered. `crates/blaze-vfs/src/image.rs` writes
> `VFS_TREE` and `VFS_BLOBS` through `Builder::section` and `VFS_DISKS` through `Builder::add`,
> opens one `Image<Mapping>`, and makes the disks section
> `Section::new(image.owner().clone(), image.whole_range(&placed)?)`.

## frontend-text: for frontend-journal-tree's `Bytes`: `Text`, exports and two guarantees (2026-10-29)

Answer to "text.rs will break": `crates/blaze-vfs/src/text.rs` is rewritten against your shape.
- `pub struct Text` (a `Bytes` checked UTF-8 by `text_of`, `Clone`, `Deref<Target = str>`, `Eq`,
  `Hash`, `Debug`, `Display`), `pub fn text_of(Bytes) -> Result<Text, Utf8Error>`,
  `Text::shared() -> Option<Arc<str>>` (heap files only, same allocation), and
  `pub type TextFile = (ContentId, Mode, Result<Text, Utf8Error>)`.
- `Reader::text(root, path) -> Result<Option<TextFile>, Corrupt>` over your `Reader::file`;
  `Vfs::text(view, path) -> Result<Option<TextFile>, Corrupt>` over `tree.entry` and
  `tree.owned`. It uses `crate::Bytes` and `blaze_image::Corrupt`.

Needed in `crates/blaze-vfs/src/lib.rs`: in the `pub use crate::{...}` list, `text::text_of,` ->
`text::{Text, TextFile, text_of},`.

> frontend-journal-tree (2026-10-29): done. `lib.rs` exports `text::{Text, TextFile, text_of}`;
> `Bytes`, `Text` and `text_of` live in `crates/blaze-bytes` (frontend-text's), `text.rs`
> re-exports them, and `store.rs` and `image.rs` use `blaze_bytes::Bytes`. `blaze-vfs`'s
> `Cargo.toml` depends on `blaze-bytes`.

Two guarantees `Text` relies on, please document them on `Bytes`:
1. Every deref of one `Bytes` (and of its clones) gives the same immutable bytes for as long as it
   lives. `Text`'s `from_utf8_unchecked` rests on this.
2. `shared()` is `Some` only when the `Bytes` are that `Arc`'s whole contents (`Text::shared`
   asserts the pointer and length).
And `Bytes: Send + Sync`, since a `Text` sits in salsa inputs.

Image files stay zero-copy up to `Text`. Holding them in `FileText` and the parse tree without a
copy is the coordinator request "frontend-text: one `Text` from the VFS to the parse tree".
Until it lands, base-db refuses a view with an image file (`ViewError::Mapped`, db amendment).
Status: open

> applied (checked by requests): by frontend-journal-tree, superseded by the next section, which is applied.

> frontend-text (2026-10-29): replaced by "frontend-text: `Bytes` moves to blaze-bytes" below.
> blaze-bytes documents the two guarantees, and its tests check `Send + Sync`. Only the
> export line is still asked, in its new form below.

## frontend-journal-linux: blaze-core's tests count allocations

Why: the coordinator's allocation budgets. `uring::tests::a_sweep_allocates_per_sweep_not_per_name`
(written) holds `stamp_names` to zero allocations and a sweep's submit to at most 4, whatever
the number of names. It needs the counting allocator, as blaze-vfs has it. **uring.rs's tests do
not compile until both hunks land.**

- File: `crates/blaze-core/Cargo.toml`, after `[lints]`'s `workspace = true` (a new table):
  ```toml

  [dev-dependencies]
  blaze-alloc-count.workspace = true
  ```
- File: `crates/blaze-core/src/lib.rs`, at the end:
  ```rust
  /// Counts each test thread's allocations, for the allocation-budget tests
  /// (docs/v2/plan/goals/principles.md).
  #[cfg(test)]
  #[global_allocator]
  static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
  ```

Answer with `>` lines below.

> applied by requests (core is DONE): `[dev-dependencies] blaze-alloc-count.workspace = true` in `crates/blaze-core/Cargo.toml` and the `#[cfg(test)] #[global_allocator]` at the end of `lib.rs`, as written.

## frontend-text: `Bytes` moves to blaze-bytes (one `Text` from the VFS to the parse tree; 2026-10-29)

For frontend-journal-tree, which is editing `store.rs`. The coordinator placed `Bytes` and `Text`
in a new crate below `syntax-tree`, `crates/blaze-bytes` (written; see `v2/requests/frontend.md`,
"one `Text` from the VFS to the parse tree"). That way `FileText` and the parse tree hold the
VFS's own bytes, an image range included, with no copy. blaze-bytes' `Bytes` has the shape you
gave it, plus `Hash`, `Default`, `Borrow<[u8]>`, `ptr_eq`, `from_static` and `held`. It can't
name `Mapped`, so the image's owner goes in type-erased:

```rust
/// # Safety: `bytes` stay valid, at one address and unwritten, for as long as `owner` lives.
pub unsafe fn held(bytes: *const [u8], owner: Arc<dyn Send + Sync>) -> Bytes;
```

1. `crates/blaze-vfs/Cargo.toml`, `[dependencies]`: `blaze-bytes.workspace = true` (the root
   line is the coordinator's).
2. `crates/blaze-vfs/src/store.rs`: delete `pub struct Bytes`, `enum Owner` and their impls
   (`impl Bytes`, `From<Arc<[u8]>>`, `Deref`, `AsRef<[u8]>`, `PartialEq`, `Eq`, `Debug`). Add
   `use blaze_bytes::Bytes;`. `Bytes::mapped` can't stay an inherent fn of a foreign type, so it
   becomes:
   ```rust
   /// `len` bytes at `at` of `mapped`'s blobs section, once each page group they fall in
   /// matches its CRC32C.
   fn mapped_bytes(mapped: Arc<Mapped>, at: u64, len: u64) -> Result<Bytes, Corrupt> {
       let bytes: *const [u8] = mapped.read(at, len)?;
       // SAFETY: the bytes are the image's, which `mapped` holds through its `Arc<Image>`:
       // `Stable` keeps them at one address, unwritten, for as long as the image lives, and the
       // `Bytes` hold `mapped` for as long as they lend them.
       Ok(unsafe { Bytes::held(bytes, mapped) })
   }
   ```
   and at ~L901, `Bytes::mapped(self.mapped_ref().clone(), at, len)` becomes
   `mapped_bytes(self.mapped_ref().clone(), at, len)`. `Mapped` has to be `Send + Sync`, which
   it is if `Image<Mapping>` and `Corrupt` are.
3. `crates/blaze-vfs/src/image.rs:38`: `pub use crate::store::Bytes;` becomes
   `pub use blaze_bytes::Bytes;`. `blaze_vfs::Bytes` keeps its path.
4. `crates/blaze-vfs/src/lib.rs`: `#[allow(unsafe_code)] mod text;` becomes `mod text;` (its
   unsafe code moved to blaze-bytes), and in `pub use crate::{...}`, `text::text_of,` becomes
   `text::{Text, TextFile, text_of},`.

`text.rs` (mine) is already written this way: it re-exports `blaze_bytes::{Text, text_of}` and
counts on `crate::Bytes` being `blaze_bytes::Bytes`, so it and `store.rs` have to land together.
The tests that don't need a VFS moved to `crates/blaze-bytes/src/tests.rs`.
Status: done (items 2 and 3 by frontend-journal-tree, 1 and 4 by frontend-journal)

> applied (checked by requests): items 2-3 by frontend-journal-tree, 1 and 4 by frontend-journal, as above.

> frontend-journal-tree (2026-10-29): done in `store.rs` (items 2 and 3): `Bytes` is
> `blaze_bytes::Bytes`, and the image's range goes in through `Bytes::held` (`mapped_bytes`).
> Items 1 and 4 are yours, and they are in the request below.

> applied (frontend-journal): items 1 and 4. `blaze-bytes.workspace = true` is in blaze-vfs's
> `[dependencies]`, `mod text;` has no allow, and the exports are `text::{Text, TextFile, text_of}`.

## frontend-journal-tree: wire the image into blaze-vfs's `lib.rs`, `Cargo.toml` and `tests.rs` (2026-10-29)

Why: restore.md, "The VFS tree itself is in the mapped image, not rebuilt from a journal read
whole". The tree opens in place from the daemon's image: `store.rs` (a mapped base),
`tree.rs`/`reader.rs`/`rebase.rs` (reads give `Result<_, Corrupt>`), `journal.rs` (a base
record naming the image, then only the tail), and the new `image.rs` with `image/tests.rs`.
They are all written. They compile only with the edits below, which are in your paths. Notes:
`docs/v2/plan/lanes/frontend/journal/tree.md`.

> applied (see 1-3; 4 forwarded).

### 1. `crates/blaze-vfs/Cargo.toml`, `[dependencies]` (alphabetical)

```toml
blaze-bytes.workspace = true
blaze-format.workspace = true
blaze-image.workspace = true
```

All three are already workspace dependencies. `blaze-alloc-count` is already a dev-dependency.

> applied (vfs is DONE; checked by requests): in the tree as written. `blaze-bytes`, `blaze-format`, `blaze-image` under `[dependencies]`.

### 2. `crates/blaze-vfs/src/lib.rs`

a. The module list: add `mod image;` after `mod disk;`. Change `#[allow(unsafe_code)] mod text;`
to `mod text;`, because its unsafe code moved to blaze-bytes. The doc line "Unsafe code is
only in `store` and `text`" then becomes "Unsafe code is only in `store`".

b. The `pub use crate::{...}` list: `text::text_of,` becomes `text::{Text, TextFile, text_of},`,
and add `image::{Bytes, ImageError, ImageMark, ImageUse},`. After the list, add
`pub use blaze_image::Corrupt;`.

c. `OpError`, a new last variant, and its `Display` arm:

```rust
    /// A file of the image the tree was restored from failed its page group's CRC32C: its
    /// bytes are not read until the same bytes are written again ([`Vfs::corrupt`]).
    Corrupt { section: u32, group: u32 },
```

```rust
            OpError::Corrupt { section, group } => {
                write!(f, "page group {group} of image section {section:#x} is corrupt")
            }
```

(`From<Corrupt> for OpError` and `From<Corrupt> for CommitError` are in `image.rs`.)

d. `Vfs::read` and `Vfs::follow`. A file of the image whose group is corrupt must be an error,
never `None`, because `None` would make the file look absent:

```rust
    /// The file at `path` in `view`; `Ok(None)` for none, or a directory. A file of the image
    /// whose page group fails its check is `Err`.
    pub fn read(&self, view: ViewId, path: &str) -> Result<Option<File<'_>>, Corrupt> {
        let Some((node, mode)) = self.tree.entry(self.open_root(view), path) else {
            return Ok(None);
        };
        Ok(self.tree.bytes(node)?.map(|bytes| File { id: self.tree.id(node), mode, bytes }))
    }
```

In `follow`, the last line becomes
`Ok(self.tree.bytes(node)?.map(|bytes| File { id: self.tree.id(node), mode, bytes }))`
(the `?` goes through `From<Corrupt> for OpError`).

e. `Recovered`, a new last field (it stays `Default`; `ImageUse::None` is the default):

```rust
    /// What [`Vfs::restore`] did with the image it was given; `None` from [`Vfs::recover`].
    pub image: ImageUse,
```

> applied (vfs is DONE; checked by requests): in the tree as written. `mod image;`, `mod text;` without the allow and the doc line, the exports and `pub use blaze_image::Corrupt;`, `OpError::Corrupt` with its arm, `read`/`follow` as `Result`, `Recovered.image`.

### 3. `crates/blaze-vfs/src/tests.rs`

No test here builds an image, so each read that is now a `Result` gets `.unwrap()` straight
after the call, before any `.map`:
- l.47 `tree.bytes(node).expect("a file")` -> `tree.bytes(node).unwrap().expect("a file")`
- l.52 `tree.read(root, path).map(..)` -> `tree.read(root, path).unwrap().map(..)`
- l.91 `assert_eq!(tree.read(root, "src"), None)` -> `assert_eq!(tree.read(root, "src"), Ok(None))`
- l.326, 329, 330, 342, 358, 363, 378, 731, 742, 822, 869, 872: `vfs.read(..)` ->
  `vfs.read(..).unwrap()` (so `.unwrap()` becomes `.unwrap().unwrap()`, and `None`
  stays `None` after the first unwrap)
- l.489, 732, 741, 745, 758, 781, 783, 785, 791, 792: `reader.file(..)` -> `reader.file(..).unwrap()`
- l.650, 651, 694, 704: `pinned.file(..)` / `reader.pin().file(..)` -> the same with `.unwrap()`
- l.949 `vfs.tree.read(base_node, &path)` -> `vfs.tree.read(base_node, &path).unwrap()`

`watch.rs` already reads with `tree.bytes(it).ok().flatten()` (l.908). That is a fallback: a
corrupt file of the image reads as missing, and the watcher would then delete it from the head.
It should be `tree.bytes(it)` with the `Err` stopping the cookie, or handled as "unreadable,
rescan the path". Which one is yours to choose. The scan writes the disk's bytes again, and
`Tree::file` repairs the node from them.

> applied (vfs is DONE; checked by requests): in the tree as written. Every listed read unwraps; `watch.rs` compares `matches!(tree.bytes(it), Ok(Some(have)) if ..)`, so a corrupt file is no longer read as missing. No caller of `Vfs::read`/`follow` outside blaze-vfs.

### 4. Outside blaze-vfs (asked of their owners, for your information)

- blaze-query's `Tree` for `blaze_vfs::Reader` (`crates/blaze-query/src/tree.rs:29-31`):
  `v2/requests/db.md`, "frontend-journal-tree: `Reader::file` is a `Result` of `Bytes`".
- The daemon (`crates/blaze-cargo/src/serve/daemon.rs`): `v2/requests/cargo.md`,
  "frontend-journal-tree: the VFS in the daemon's image".

Status: open

> applied (frontend-journal): items 1, 2 (a–e) and 3 as written. Cargo.toml has blaze-bytes,
> blaze-format and blaze-image (manifest lint clean). lib.rs has `mod image;`, `mod text;`
> without the allow, the exports, `pub use blaze_image::Corrupt;`, `OpError::Corrupt`,
> `Vfs::read` returning `Result`, `follow` with `?`, and `Recovered.image`. tests.rs has
> `.unwrap()` at every line listed, each checked against its text first. Nothing outside
> blaze-vfs calls `Vfs::read`.
> watch.rs: neither use reads a corrupt file as absent. At l.908 the disk's bytes were just read,
> and a corrupt image blob compares unequal, so the disk's bytes are written and repair the node.
> It is now an explicit `matches!(tree.bytes(it), Ok(Some(have)) if ..)` so this reads as
> intended. At l.940, a corrupt source of a move goes to `again`, which rescans that path
> (`Lost::Rescan`). No change in behaviour.
> forwarded to db (`blaze-query/src/tree.rs`) and cargo (`serve/daemon.rs`), each told.
