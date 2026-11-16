//! Linux's roots: PEM bundles and directories, as OpenSSL finds them.
//!
//! `SSL_CERT_FILE` and `SSL_CERT_DIR` (a `:`-separated list), when either is set, are the only
//! places read, as with OpenSSL: a run that names its roots gets those. Otherwise the
//! distributions' standard bundles are read, the first that exists, then `/etc/ssl/certs` when
//! none does. A place that is there but broken is an error, never passed over.

use std::{
    alloc::{Allocator, Global},
    ffi::OsStr,
    io::Read as _,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use super::{Looked, MOST_FILE, Place, Places, Roots, TrustError, Why, pem};

/// The standard bundles, in the order looked: Debian and Ubuntu, Fedora and RHEL, openSUSE,
/// OpenELEC, CentOS and RHEL's extracted store, Alpine.
pub const BUNDLES: [&str; 6] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/pki/tls/cacert.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

/// The standard directory, read when no bundle exists.
pub const DIR: &str = "/etc/ssl/certs";

/// The most entries of one directory read.
const MOST_ENTRIES: usize = 100_000;

/// The roots of `places`, or of the standard places when neither is set.
pub fn roots<A: Allocator + Clone>(places: Places<'_>, alloc: A) -> Result<Roots<A>, TrustError> {
    let mut roots = Roots::new_in(alloc);
    let mut looked = Vec::new();
    let named = places.cert_file.is_some() || places.cert_dir.is_some();
    if named {
        if let Some(file) = places.cert_file {
            let path = PathBuf::from(file);
            let why = read_file(&path, &mut roots);
            looked.push(Looked { place: Place::File { path, from: Some("SSL_CERT_FILE") }, why });
        }
        if let Some(dirs) = places.cert_dir {
            for dir in dirs.as_bytes().split(|it| *it == b':').filter(|it| !it.is_empty()) {
                let path = PathBuf::from(OsStr::from_bytes(dir));
                let why = read_dir(&path, &mut roots);
                looked.push(Looked { place: Place::Dir { path, from: Some("SSL_CERT_DIR") }, why });
            }
        }
    } else {
        for bundle in BUNDLES {
            let path = PathBuf::from(bundle);
            let why = read_file(&path, &mut roots);
            let found = !matches!(why, Why::Absent);
            looked.push(Looked { place: Place::File { path, from: None }, why });
            if found {
                break;
            }
        }
        if looked.iter().all(|it| it.why == Why::Absent) {
            let path = PathBuf::from(DIR);
            let why = read_dir(&path, &mut roots);
            looked.push(Looked { place: Place::Dir { path, from: None }, why });
        }
    }
    // A place there but broken fails the read: a store read in part is not the store.
    let broken = looked.iter().any(|it| !matches!(it.why, Why::Absent | Why::Gave(_)));
    if broken || roots.is_empty() {
        return Err(TrustError { looked });
    }
    Ok(roots)
}

/// One bundle's certificates into `roots`.
fn read_file<A: Allocator>(path: &Path, roots: &mut Roots<A>) -> Why {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Why::Absent,
        Err(e) => return Why::Unreadable(e.kind()),
    };
    let mut text = Vec::new_in(Global);
    match file.take(MOST_FILE + 1).read_to_end(&mut text) {
        Ok(_) if text.len() as u64 > MOST_FILE => return Why::TooBig { len: text.len() as u64 },
        Ok(_) => {}
        Err(e) => return Why::Unreadable(e.kind()),
    }
    match pem::certificates(&text, roots, Global) {
        Ok(0) => Why::NoCertificate,
        Ok(n) => Why::Gave(n),
        Err(e) => Why::Pem(e),
    }
}

/// Every PEM file of one directory into `roots`. Entries that are not regular files once
/// links are followed (a dangling hash link) and files with no certificate are passed over,
/// as OpenSSL does; a broken certificate is not.
fn read_dir<A: Allocator>(path: &Path, roots: &mut Roots<A>) -> Why {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Why::Absent,
        Err(e) => return Why::Unreadable(e.kind()),
    };
    let mut names = Vec::new_in(Global);
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.path()),
            Err(e) => return Why::Unreadable(e.kind()),
        }
        if names.len() > MOST_ENTRIES {
            return Why::Over(super::Over::Roots);
        }
    }
    // The system's order is the directory's; sorted, two reads add roots in one order.
    names.sort_unstable();
    let mut gave = 0u32;
    for name in &names {
        if !std::fs::metadata(name).is_ok_and(|it| it.is_file()) {
            continue;
        }
        match read_file(name, roots) {
            Why::Gave(n) => gave += n,
            Why::NoCertificate | Why::Absent => {}
            other => return other,
        }
    }
    match gave {
        0 => Why::NoCertificate,
        n => Why::Gave(n),
    }
}
