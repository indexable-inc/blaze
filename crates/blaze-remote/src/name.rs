//! Object names. Every name in the bucket is built here from parts that are validated or that
//! are digests. Nothing read from the bucket becomes a path except a digest's hex, so no name
//! can leave its namespace or climb out of its directory.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::fmt;

use blaze_cas::plain::Namespace;
use blaze_ids::ContentId;

use crate::wire::hex;

/// The longest part of a name.
pub const PART_MOST: usize = 64;

/// A part of a name that is refused, with why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BadName {
    /// What the part is: "toolchain", "target", "cfg", "root".
    pub what: &'static str,
    pub name: String,
    pub why: &'static str,
}

impl fmt::Display for BadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the {} name {:?} is refused: {} (a name part is [0-9a-z._-], 1 to {PART_MOST} bytes, not starting with '.')", self.what, self.name, self.why)
    }
}

/// Checks one part of a name: `[0-9a-z._-]{1,64}`, never starting with `.` (so never `.` or
/// `..`), never holding `/`.
pub fn part(what: &'static str, name: &str) -> Result<(), BadName> {
    let bad = |why| Err(BadName { what, name: name.to_owned(), why });
    if name.is_empty() {
        return bad("it is empty");
    }
    if name.len() > PART_MOST {
        return bad("it is too long");
    }
    if name.starts_with('.') {
        return bad("it starts with '.'");
    }
    if !name.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'.' || b == b'_' || b == b'-') {
        return bad("it holds a byte outside [0-9a-z._-]");
    }
    Ok(())
}

/// A toolchain identity: the pinned nightly and blaze's own identity, as one name part.
/// Everything under a namespace except key lists is under one, so a toolchain's objects can
/// expire together ([`crate::expire`]).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Toolchain(String);

impl Toolchain {
    pub fn new(name: &str) -> Result<Toolchain, BadName> {
        part("toolchain", name)?;
        Ok(Toolchain(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// What a statement binds: the digest of the name.
    pub fn digest(&self) -> ContentId {
        ContentId::of("blaze-remote toolchain 1", self.0.as_bytes())
    }
}

/// The namespace's directory, or a refusal for [`Namespace::LOCAL`], which never leaves the
/// machine.
pub fn space(namespace: Namespace) -> Result<String, BadName> {
    if namespace == Namespace::LOCAL {
        return Err(BadName { what: "namespace", name: "local".to_owned(), why: "the local namespace never leaves this machine" });
    }
    Ok(if namespace == Namespace::PUBLIC { "public".to_owned() } else { format!("{:016x}", namespace.0) })
}

/// The namespace a settings file names: `public`, or an organisation's name hashed as the
/// store hashes it ([`Namespace::named`]).
pub fn namespace_named(name: &str) -> Result<Namespace, BadName> {
    part("namespace", name)?;
    Ok(match name {
        "public" => Namespace::PUBLIC,
        "local" => return Err(BadName { what: "namespace", name: name.to_owned(), why: "the local namespace never leaves this machine" }),
        _ => Namespace::named(name),
    })
}

fn under(namespace: Namespace, toolchain: &Toolchain) -> Result<String, BadName> {
    Ok(format!("{}/t/{}", space(namespace)?, toolchain.as_str()))
}

pub fn keys(namespace: Namespace, serial: u64) -> Result<String, BadName> {
    Ok(format!("{}/keys/{serial:016x}", space(namespace)?))
}

pub fn segment(namespace: Namespace, toolchain: &Toolchain, digest: &ContentId) -> Result<String, BadName> {
    Ok(format!("{}/segments/{}", under(namespace, toolchain)?, hex(digest)))
}

pub fn statement(namespace: Namespace, toolchain: &Toolchain, digest: &ContentId) -> Result<String, BadName> {
    Ok(format!("{}/segments/{}.statement", under(namespace, toolchain)?, hex(digest)))
}

pub fn shard(namespace: Namespace, toolchain: &Toolchain, generation: u64, digest: &ContentId) -> Result<String, BadName> {
    Ok(format!("{}/index/{generation:016x}/{}", under(namespace, toolchain)?, hex(digest)))
}

pub fn root(namespace: Namespace, toolchain: &Toolchain, name: &str) -> Result<String, BadName> {
    part("root", name)?;
    Ok(format!("{}/roots/{name}", under(namespace, toolchain)?))
}

pub fn bundle(namespace: Namespace, toolchain: &Toolchain, target: &str, cfg: &str) -> Result<String, BadName> {
    part("target", target)?;
    part("cfg", cfg)?;
    Ok(format!("{}/bundles/{target}/{cfg}", under(namespace, toolchain)?))
}

/// The prefix of everything of one toolchain in a namespace: what expiry deletes.
pub fn toolchain_prefix(namespace: Namespace, toolchain: &Toolchain) -> Result<String, BadName> {
    Ok(format!("{}/", under(namespace, toolchain)?))
}

/// Whether `name` is under the namespace `namespace`: the oracle's check that nothing of one
/// namespace was written under another.
pub fn is_under(name: &str, namespace: Namespace) -> bool {
    match space(namespace) {
        Ok(space) => name.strip_prefix(&space).is_some_and(|rest| rest.starts_with('/')),
        Err(_) => false,
    }
}
